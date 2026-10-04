//! Managed sources that keep failing to refresh from their remote.
//!
//! A failed refresh of an existing `github:repo` checkout or `github:release`
//! install is deliberately a warning: the installed copy still works, and a
//! network blip must not turn a cron update red. The cost is that a source
//! stuck for good (diverged, dirty, unreachable remote) looked exactly like a
//! one-off blip. This module owns the two pieces that make the stuck case
//! visible without changing that severity:
//!
//! - the `<name>.repo.pull-failure` record `update` writes when a managed
//!   checkout cannot be refreshed and removes after the next success; and
//! - the read-only staleness policy behind the `stale-remote` health kind.
//!
//! Staleness compares remote-check stamps with their peers rather than the
//! wall clock: a laptop that slept for a week has old stamps everywhere, which
//! is not a problem, while one dependency whose stamp trails every other by
//! more than a day keeps failing while its peers succeed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::state;

/// Suffix of the per-dependency pull-failure record in the state directory.
pub const RECORD_SUFFIX: &str = "repo.pull-failure";

/// How far a dependency's last successful remote check may trail its peers
/// (on top of the remote TTL, which legitimately staggers stamps) before
/// health reports it. A day rides out an outage of GitHub or of one host's
/// network without staying quiet about a source that never recovers.
pub const STALE_AFTER_SECS: u64 = 24 * 60 * 60;

/// Stamps further ahead of `now` than this are clock damage, not evidence
/// that a peer refreshed recently. Matches the stamp freshness tolerance.
const FUTURE_TOLERANCE_SECS: u64 = 300;

/// Consecutive failures a streak needs before its span alone counts as
/// stale. One failure before a laptop sleeps and one from the catch-up run at
/// wake, before the network is back, span the whole sleep but prove nothing.
const MIN_STREAK_FAILURES: u64 = 3;

/// Bound on the persisted and displayed Git message. One line of context is
/// the point; a full transcript belongs in a manual `git fetch`.
const DETAIL_MAX_CHARS: usize = 200;

/// Why the last refresh of a managed checkout failed. Persisted by token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `git fetch` failed on every origin it was allowed to try.
    Fetch,
    /// Origin answered but no longer has the branch the checkout tracks
    /// (deleted, or the default branch was renamed), so the shallow clone's
    /// single-branch fetch can never succeed again.
    UpstreamGone,
    /// The checkout has commits its upstream does not (local commits, or the
    /// upstream history was rewritten), so it cannot fast-forward.
    Diverged,
    /// Local edits block the fast-forward.
    Dirty,
    /// The fast-forward failed for another reason Git reported.
    Merge,
    /// The fast-forward failed and `git status` could not classify why.
    Status,
}

impl Reason {
    /// Stable token written to the record.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::UpstreamGone => "upstream-gone",
            Self::Diverged => "diverged",
            Self::Dirty => "dirty",
            Self::Merge => "merge",
            Self::Status => "status",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        [
            Self::Fetch,
            Self::UpstreamGone,
            Self::Diverged,
            Self::Dirty,
            Self::Merge,
            Self::Status,
        ]
        .into_iter()
        .find(|reason| reason.token() == token)
    }
}

/// One failed refresh: the class plus Git's own first line of explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Failure class; drives the remediation hint.
    pub reason: Reason,
    /// Sanitized first line of Git's stderr, possibly empty.
    pub detail: String,
}

impl Failure {
    /// Builds a failure from raw Git stderr.
    #[must_use]
    pub fn new(reason: Reason, stderr: &str) -> Self {
        Self {
            reason,
            detail: first_line(stderr),
        }
    }

    /// Human-readable cause, e.g. `fetch failed: Could not resolve host`.
    #[must_use]
    pub fn cause(&self) -> String {
        let with_detail = |label: &str| {
            if self.detail.is_empty() {
                label.to_owned()
            } else {
                format!("{label}: {}", self.detail)
            }
        };
        match self.reason {
            Reason::Fetch => with_detail("fetch failed"),
            Reason::UpstreamGone => with_detail("upstream branch deleted"),
            Reason::Diverged => "diverged from origin".to_owned(),
            Reason::Dirty => "dirty working tree".to_owned(),
            Reason::Merge => with_detail("fast-forward failed"),
            Reason::Status => "status unavailable".to_owned(),
        }
    }
}

/// A persisted pull-failure record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Epoch seconds of the first failure in the current unbroken streak.
    pub since: u64,
    /// Epoch seconds of the most recent failure.
    pub last: u64,
    /// Consecutive failures in the streak, at least 1.
    pub failures: u64,
    /// The most recent failure.
    pub failure: Failure,
}

/// Returns a dependency's pull-failure record path.
#[must_use]
pub fn record_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.{RECORD_SUFFIX}"))
}

/// Records a failed refresh at `now`, continuing the current streak.
///
/// The format is `key=value` lines (`since`, `last`, `failures`, `reason`,
/// `detail`); readers ignore unknown keys so later fields stay compatible.
pub fn record_failure(state_dir: &Path, name: &str, failure: &Failure, now: u64) -> Result<()> {
    let path = record_path(state_dir, name);
    let last_success = read_stamp(state_dir, name, Source::Repo);
    // The streak restarts when the previous record is from the future (the
    // clock moved backward) or predates a success that failed to remove it
    // (an older Shdeps, or a failed removal); otherwise it would anchor at a
    // time before that success forever and `find` would ignore it.
    let previous = read(&path).filter(|previous| {
        previous.since <= now && last_success.is_none_or(|success| success <= previous.since)
    });
    let (since, failures) = previous.map_or((now, 1), |previous| {
        (previous.since, previous.failures.saturating_add(1))
    });
    state::write_atomic(
        &path,
        &format!(
            "since={since}\nlast={now}\nfailures={failures}\nreason={}\ndetail={}\n",
            failure.reason.token(),
            failure.detail
        ),
    )
}

/// Removes the record after a successful refresh. Absence is success.
pub fn clear(state_dir: &Path, name: &str) -> io::Result<()> {
    match fs::remove_file(record_path(state_dir, name)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Reads a record. A missing, unreadable, or malformed record reads as
/// absent: it is a diagnostic aid, and a damaged one must not make health
/// report more than the stamps already prove.
#[must_use]
pub fn read(path: &Path) -> Option<Record> {
    let text = fs::read_to_string(path).ok()?;
    let (mut since, mut last, mut failures, mut reason, mut detail) =
        (None, None, None, None, String::new());
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "since" => since = value.parse().ok(),
            "last" => last = value.parse().ok(),
            "failures" => failures = value.parse().ok(),
            "reason" => reason = Reason::parse(value),
            "detail" => detail = first_line(value),
            _ => {}
        }
    }
    let since = since?;
    Some(Record {
        since,
        last: last.unwrap_or(since).max(since),
        failures: failures.unwrap_or(1).max(1),
        failure: Failure {
            reason: reason?,
            detail,
        },
    })
}

/// Which remote stamp a dependency refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A `github:repo` checkout (`<name>.repo.stamp`).
    Repo,
    /// A `github:release` install (`<name>.release.stamp`).
    Release,
}

impl Source {
    const fn stamp_kind(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Release => "release",
        }
    }
}

/// One dependency the staleness check considers.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Dependency name.
    pub name: String,
    /// Which stamp it refreshes.
    pub source: Source,
    /// Install root, reported as the affected path.
    pub root: PathBuf,
    /// Whether a stale stamp is this dependency's problem. A development
    /// clone is user-owned: `update` skips its pull while it has local edits,
    /// so its stamp legitimately trails. It still counts as a peer.
    pub subject: bool,
}

/// One dependency that has not refreshed for too long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    /// Dependency name.
    pub name: String,
    /// Install root.
    pub root: PathBuf,
    /// One-line explanation ending in a remediation hint.
    pub detail: String,
}

/// Finds dependencies whose last successful remote check (or, for a
/// checkout with a pull-failure record, its first failure since then) trails
/// their peers by more than [`STALE_AFTER_SECS`] plus `ttl`, or whose
/// pull-failure streak itself spans that long (which also covers a host
/// where every source fails, so no peer is newer). Costs one small read per candidate stamp and one
/// per repo record; never touches the network.
#[must_use]
pub fn find(state_dir: &Path, candidates: &[Candidate], now: u64, ttl: u64) -> Vec<Stale> {
    let threshold = STALE_AFTER_SECS.saturating_add(ttl);
    let stamps = candidates
        .iter()
        .map(|candidate| read_stamp(state_dir, &candidate.name, candidate.source))
        .collect::<Vec<_>>();
    // The newest believable stamp of any kind: release and repo checks run in
    // the same update, and comparing across kinds catches the case that
    // motivated this check, every checkout failing over SSH on a host without
    // a GitHub key while release checks over HTTPS keep succeeding.
    let reference = stamps
        .iter()
        .flatten()
        .copied()
        .filter(|stamp| *stamp <= now.saturating_add(FUTURE_TOLERANCE_SECS))
        .max();

    let mut stale = Vec::new();
    for (candidate, stamp) in candidates.iter().zip(stamps) {
        if !candidate.subject {
            continue;
        }
        // A record older than the last success outlived it (its removal
        // failed, or an older Shdeps that never removes it ran since), so it
        // says nothing about the current state.
        let record = (candidate.source == Source::Repo)
            .then(|| read(&record_path(state_dir, &candidate.name)))
            .flatten()
            .filter(|record| stamp.is_none_or(|success| success <= record.since));
        // With a record, the lag runs from the first failure, not the last
        // success: between the two the checkout was not due or the host was
        // asleep or offline, so a stamp that trails its peers by a week after
        // one failed catch-up run is not a week of failing while they
        // succeeded. A surviving record is never older than the stamp.
        let failing_since = record.as_ref().map(|record| record.since).or(stamp);
        let peer_lag = match (reference, failing_since) {
            (Some(reference), Some(since)) => reference.saturating_sub(since),
            _ => 0,
        };
        let streak = record
            .as_ref()
            .filter(|record| record.failures >= MIN_STREAK_FAILURES)
            .map_or(0, |record| record.last.saturating_sub(record.since));
        let lag = peer_lag.max(streak);
        if lag <= threshold {
            continue;
        }
        stale.push(Stale {
            name: candidate.name.clone(),
            root: candidate.root.clone(),
            detail: stale_detail(candidate, record.as_ref(), lag),
        });
    }
    stale
}

fn read_stamp(state_dir: &Path, name: &str, source: Source) -> Option<u64> {
    let path = crate::stamp::remote_path(state_dir, name, source.stamp_kind());
    fs::read_to_string(path).ok()?.trim_end().parse().ok()
}

fn stale_detail(candidate: &Candidate, record: Option<&Record>, lag: u64) -> String {
    let age = format_age(lag);
    let root = candidate.root.display();
    match (candidate.source, record) {
        // A transient release-metadata failure prints only the installed
        // version, so point at what usually causes it.
        (Source::Release, _) => format!(
            "release has not been checked successfully for {age} while other dependencies were; check access to the GitHub API (set GH_TOKEN if rate-limited), then run 'shdeps --force update'"
        ),
        (Source::Repo, None) => format!(
            "checkout has not refreshed for {age} while other dependencies did; run 'shdeps update' to see why"
        ),
        (Source::Repo, Some(record)) => {
            let hint = match record.failure.reason {
                Reason::Fetch => format!(
                    "check network and GitHub access with 'git -C {root} fetch', then run 'shdeps update'"
                ),
                Reason::Diverged => {
                    format!("move {root} aside and run 'shdeps update' to clone it again")
                }
                Reason::UpstreamGone => format!(
                    "move {root} aside and run 'shdeps update' to clone the repository's current default branch"
                ),
                Reason::Dirty => format!(
                    "review 'git -C {root} status', discard the edits, then run 'shdeps update'"
                ),
                Reason::Merge | Reason::Status => format!(
                    "inspect {root}, or move it aside and run 'shdeps update' to clone it again"
                ),
            };
            format!(
                "checkout has failed to refresh for {age} ({}); {hint}",
                record.failure.cause()
            )
        }
    }
}

fn format_age(secs: u64) -> String {
    let hours = secs / 3600;
    if hours < 48 {
        format!("{hours}h")
    } else {
        format!("{}d", hours / 24)
    }
}

/// Reduces Git stderr to its first informative line, safe to persist and
/// print: `hint:` and warning lines (ssh's "Permanently added" notice comes
/// before the real error) and `fatal:`/`error:` prefixes are dropped, URL
/// credentials are redacted, control and bidirectional-format characters (a
/// remote can put them in its messages) become spaces, a trailing period is
/// dropped so the cause reads inside parentheses, and the result is bounded.
#[must_use]
pub fn first_line(stderr: &str) -> String {
    let informative = |line: &&str| {
        let lower = line.to_ascii_lowercase();
        !line.is_empty() && !lower.starts_with("hint:") && !lower.starts_with("warning:")
    };
    let line = stderr
        .lines()
        .map(str::trim)
        .find(informative)
        .unwrap_or("");
    let line = ["fatal: ", "error: "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .unwrap_or(line);
    let cleaned = redact_credentials(line)
        .chars()
        .map(|ch| if unsafe_char(ch) { ' ' } else { ch })
        .collect::<String>();
    let cleaned = cleaned.trim().trim_end_matches('.').trim_end();
    let mut bounded = cleaned.chars().take(DETAIL_MAX_CHARS).collect::<String>();
    if cleaned.chars().count() > DETAIL_MAX_CHARS {
        bounded.push_str("...");
    }
    bounded
}

/// Control characters plus the Unicode bidirectional controls, which can
/// make a printed line read differently from its bytes.
fn unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Replaces the userinfo of every `scheme://user:secret@host` in `text`.
/// An `insteadOf` rewrite can put a token into the URL Git prints.
fn redact_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (head, tail) = rest.split_at(index + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|ch: char| ch == '/' || ch == '\'' || ch == '"' || ch.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{
        Candidate, Failure, Reason, STALE_AFTER_SECS, Source, clear, find, first_line, read,
        record_failure, record_path,
    };

    const NOW: u64 = 1_700_000_000;
    const DAY: u64 = 24 * 60 * 60;

    fn candidate(name: &str, source: Source, subject: bool) -> Candidate {
        Candidate {
            name: name.to_owned(),
            source,
            root: PathBuf::from(format!("/share/{name}")),
            subject,
        }
    }

    fn stamp(state: &Path, name: &str, kind: &str, at: u64) {
        crate::stamp::remote_touch(&crate::stamp::remote_path(state, name, kind), at).unwrap();
    }

    #[test]
    fn first_line_skips_hints_and_prefixes_and_redacts_credentials() {
        assert_eq!(
            first_line(
                "\nhint: something\nfatal: unable to access 'https://me:tok3n@github.com/o/r/': Could not resolve host: github.com\nmore\n"
            ),
            "unable to access 'https://***@github.com/o/r/': Could not resolve host: github.com"
        );
        assert_eq!(first_line("error: x\u{1b}[31my\u{202e}z"), "x [31my z");
        assert_eq!(
            first_line(
                "Warning: Permanently added 'github.com' to the list of known hosts.\ngit@github.com: Permission denied (publickey).\n"
            ),
            "git@github.com: Permission denied (publickey)"
        );
        assert_eq!(first_line(""), "");
        let long = "a".repeat(500);
        assert_eq!(first_line(&long).chars().count(), 203);
    }

    #[test]
    fn record_keeps_streak_start_and_clear_removes_it() {
        let state = crate::test_support::temp_dir("stale-remote-record");
        let failure = Failure::new(Reason::Fetch, "fatal: boom\n");

        record_failure(&state, "owner/tool", &failure, NOW).unwrap();
        let later = Failure::new(Reason::Diverged, "");
        record_failure(&state, "owner/tool", &later, NOW + 60).unwrap();

        let path = record_path(&state, "owner/tool");
        assert_eq!(path, state.join("owner/tool.repo.pull-failure"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!(
                "since={NOW}\nlast={}\nfailures=2\nreason=diverged\ndetail=\n",
                NOW + 60
            )
        );
        assert_eq!(read(&path).unwrap().failure, later);

        clear(&state, "owner/tool").unwrap();
        assert!(!path.exists());
        clear(&state, "owner/tool").unwrap();
    }

    #[test]
    fn record_from_the_future_restarts_the_streak() {
        let state = crate::test_support::temp_dir("stale-remote-future-record");
        let failure = Failure::new(Reason::Fetch, "");
        record_failure(&state, "tool", &failure, NOW + DAY).unwrap();

        record_failure(&state, "tool", &failure, NOW).unwrap();

        assert_eq!(read(&record_path(&state, "tool")).unwrap().since, NOW);
    }

    #[test]
    fn success_after_the_record_restarts_the_streak() {
        // An older Shdeps refreshed the checkout without removing the record;
        // the next failure must start a new streak, not inherit the old one.
        let state = crate::test_support::temp_dir("stale-remote-restart");
        let failure = Failure::new(Reason::Fetch, "");
        record_failure(&state, "tool", &failure, NOW - 5 * DAY).unwrap();
        stamp(&state, "tool", "repo", NOW - 60);

        record_failure(&state, "tool", &failure, NOW).unwrap();

        let record = read(&record_path(&state, "tool")).unwrap();
        assert_eq!((record.since, record.failures), (NOW, 1));
    }

    #[test]
    fn malformed_record_reads_as_absent() {
        let state = crate::test_support::temp_dir("stale-remote-malformed");
        let path = record_path(&state, "tool");
        fs::write(&path, "since=soon\nreason=fetch\n").unwrap();
        assert!(read(&path).is_none());
        fs::write(&path, "since=1\nreason=unknown\n").unwrap();
        assert!(read(&path).is_none());
    }

    #[test]
    fn stamp_trailing_peers_beyond_threshold_is_stale() {
        let state = crate::test_support::temp_dir("stale-remote-lag");
        stamp(&state, "peer", "release", NOW);
        stamp(&state, "tool", "repo", NOW - STALE_AFTER_SECS - 3600 - 1);
        stamp(&state, "fine", "repo", NOW - STALE_AFTER_SECS - 3600);
        let candidates = [
            candidate("peer", Source::Release, true),
            candidate("tool", Source::Repo, true),
            candidate("fine", Source::Repo, true),
        ];

        let stale = find(&state, &candidates, NOW, 3600);

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "tool");
        assert_eq!(
            stale[0].detail,
            "checkout has not refreshed for 25h while other dependencies did; run 'shdeps update' to see why"
        );
    }

    #[test]
    fn host_asleep_for_days_is_not_stale() {
        // Every stamp is old, none trails another: comparing peers instead
        // of the clock keeps a laptop that slept a week quiet.
        let state = crate::test_support::temp_dir("stale-remote-asleep");
        stamp(&state, "a", "repo", NOW - 7 * DAY);
        stamp(&state, "b", "release", NOW - 7 * DAY + 60);

        let candidates = [
            candidate("a", Source::Repo, true),
            candidate("b", Source::Release, true),
        ];
        assert!(find(&state, &candidates, NOW, 3600).is_empty());
    }

    #[test]
    fn single_failure_after_a_long_sleep_is_not_stale() {
        // The first run after a week asleep refreshed the peers but failed
        // this checkout once. The week its stamp trails them is sleep, not a
        // week of failing while they succeeded.
        let state = crate::test_support::temp_dir("stale-remote-woke-failing");
        stamp(&state, "peer", "release", NOW);
        stamp(&state, "tool", "repo", NOW - 7 * DAY);
        record_failure(
            &state,
            "tool",
            &Failure::new(Reason::Fetch, "boom"),
            NOW - 60,
        )
        .unwrap();

        let candidates = [
            candidate("peer", Source::Release, true),
            candidate("tool", Source::Repo, true),
        ];
        assert!(find(&state, &candidates, NOW, 3600).is_empty());
    }

    #[test]
    fn failing_since_a_long_sleep_is_stale_once_peers_succeed_for_a_day() {
        // The same checkout still failing a day and a TTL after waking, while
        // its peers keep refreshing, is stuck; the age counts from the first
        // failure, not from the last success before the sleep.
        let state = crate::test_support::temp_dir("stale-remote-woke-stuck");
        stamp(&state, "peer", "release", NOW);
        stamp(&state, "tool", "repo", NOW - 9 * DAY);
        let failure = Failure::new(Reason::Fetch, "boom");
        record_failure(&state, "tool", &failure, NOW - 2 * DAY).unwrap();
        record_failure(&state, "tool", &failure, NOW).unwrap();

        let candidates = [
            candidate("peer", Source::Release, true),
            candidate("tool", Source::Repo, true),
        ];
        let stale = find(&state, &candidates, NOW, 3600);

        assert_eq!(
            stale[0].detail,
            "checkout has failed to refresh for 2d (fetch failed: boom); check network and GitHub access with 'git -C /share/tool fetch', then run 'shdeps update'"
        );
    }

    #[test]
    fn record_cause_and_hint_name_the_failure() {
        let state = crate::test_support::temp_dir("stale-remote-cause");
        stamp(&state, "peer", "repo", NOW);
        stamp(&state, "tool", "repo", NOW - 3 * DAY - 3600);
        record_failure(
            &state,
            "tool",
            &Failure::new(Reason::Fetch, "fatal: Could not resolve host: github.com"),
            NOW - 3 * DAY,
        )
        .unwrap();
        let candidates = [
            candidate("peer", Source::Repo, true),
            candidate("tool", Source::Repo, true),
        ];

        let stale = find(&state, &candidates, NOW, 3600);

        assert_eq!(
            stale[0].detail,
            "checkout has failed to refresh for 3d (fetch failed: Could not resolve host: github.com); check network and GitHub access with 'git -C /share/tool fetch', then run 'shdeps update'"
        );
    }

    #[test]
    fn deleted_upstream_branch_hint_points_at_a_new_clone() {
        // Checking the network is the wrong next step when origin answered
        // without the tracked branch; only a new clone picks up the renamed
        // default branch.
        let state = crate::test_support::temp_dir("stale-remote-upstream-gone");
        stamp(&state, "peer", "repo", NOW);
        stamp(&state, "tool", "repo", NOW - 3 * DAY - 60);
        record_failure(
            &state,
            "tool",
            &Failure::new(
                Reason::UpstreamGone,
                "fatal: couldn't find remote ref refs/heads/master",
            ),
            NOW - 3 * DAY,
        )
        .unwrap();
        let candidates = [
            candidate("peer", Source::Repo, true),
            candidate("tool", Source::Repo, true),
        ];

        let stale = find(&state, &candidates, NOW, 3600);

        assert!(
            fs::read_to_string(record_path(&state, "tool"))
                .unwrap()
                .contains("\nreason=upstream-gone\n")
        );
        assert_eq!(
            stale[0].detail,
            "checkout has failed to refresh for 3d (upstream branch deleted: couldn't find remote ref refs/heads/master); move /share/tool aside and run 'shdeps update' to clone the repository's current default branch"
        );
    }

    #[test]
    fn failure_streak_alone_is_stale_when_no_peer_is_newer() {
        // Every checkout failing (no SSH key, all origins on SSH) leaves no
        // newer peer; the record's own streak still proves a day of failures.
        let state = crate::test_support::temp_dir("stale-remote-streak");
        stamp(&state, "tool", "repo", NOW - 3 * DAY);
        let failure = Failure::new(Reason::Diverged, "");
        record_failure(&state, "tool", &failure, NOW - 2 * DAY).unwrap();
        record_failure(&state, "tool", &failure, NOW - DAY).unwrap();
        record_failure(&state, "tool", &failure, NOW).unwrap();

        let stale = find(&state, &[candidate("tool", Source::Repo, true)], NOW, 3600);

        assert_eq!(
            stale[0].detail,
            "checkout has failed to refresh for 2d (diverged from origin); move /share/tool aside and run 'shdeps update' to clone it again"
        );
    }

    #[test]
    fn two_failures_bracketing_a_sleep_are_not_a_streak() {
        let state = crate::test_support::temp_dir("stale-remote-sleep-streak");
        stamp(&state, "tool", "repo", NOW - 3 * DAY - 60);
        let failure = Failure::new(Reason::Fetch, "");
        record_failure(&state, "tool", &failure, NOW - 3 * DAY).unwrap();
        record_failure(&state, "tool", &failure, NOW).unwrap();

        assert!(find(&state, &[candidate("tool", Source::Repo, true)], NOW, 3600).is_empty());
    }

    #[test]
    fn record_older_than_the_last_success_is_ignored() {
        let state = crate::test_support::temp_dir("stale-remote-outlived");
        let failure = Failure::new(Reason::Fetch, "boom");
        record_failure(&state, "tool", &failure, NOW - 5 * DAY).unwrap();
        record_failure(&state, "tool", &failure, NOW - 2 * DAY).unwrap();
        stamp(&state, "tool", "repo", NOW - 60);

        assert!(find(&state, &[candidate("tool", Source::Repo, true)], NOW, 3600).is_empty());
    }

    #[test]
    fn development_clone_is_a_peer_but_never_a_subject() {
        let state = crate::test_support::temp_dir("stale-remote-dev");
        stamp(&state, "dev", "repo", NOW - 9 * DAY);
        stamp(&state, "peer", "repo", NOW);
        stamp(&state, "tool", "repo", NOW - 3 * DAY);
        let candidates = [
            candidate("dev", Source::Repo, false),
            candidate("peer", Source::Repo, false),
            candidate("tool", Source::Repo, true),
        ];

        let stale = find(&state, &candidates, NOW, 3600);

        assert_eq!(
            stale.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["tool"]
        );
    }

    #[test]
    fn future_dated_peer_stamp_is_not_a_reference() {
        let state = crate::test_support::temp_dir("stale-remote-future-peer");
        stamp(&state, "skewed", "release", NOW + 30 * DAY);
        stamp(&state, "tool", "repo", NOW - 60);

        let candidates = [
            candidate("skewed", Source::Release, true),
            candidate("tool", Source::Repo, true),
        ];
        assert!(find(&state, &candidates, NOW, 3600).is_empty());
    }

    #[test]
    fn release_staleness_has_its_own_wording() {
        let state = crate::test_support::temp_dir("stale-remote-release");
        stamp(&state, "peer", "repo", NOW);
        stamp(&state, "rel", "release", NOW - 4 * DAY);
        let candidates = [
            candidate("peer", Source::Repo, true),
            candidate("rel", Source::Release, true),
        ];

        let stale = find(&state, &candidates, NOW, 3600);

        assert_eq!(
            stale[0].detail,
            "release has not been checked successfully for 4d while other dependencies were; check access to the GitHub API (set GH_TOKEN if rate-limited), then run 'shdeps --force update'"
        );
    }
}
