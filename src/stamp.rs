//! Remote TTL and git revision stamp files.
//!
//! Stamps are deliberately tiny text files because they sit on the no-op
//! update path. A fresh stamp lets `shdeps` skip network/API work; force and
//! reinstall modes always bypass freshness so explicit user intent wins over
//! cached state.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::state;

/// Runtime flags that affect stamp freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// Current epoch seconds.
    pub now: u64,
    /// Remote-cache TTL in seconds.
    pub ttl: u64,
    /// Force mode bypasses all remote stamps.
    pub force: bool,
    /// Reinstall mode bypasses all remote stamps.
    pub reinstall: bool,
}

/// Returns a dependency's remote-check stamp path.
#[must_use]
pub fn remote_path(state_dir: &Path, name: &str, kind: &str) -> PathBuf {
    state_dir.join(format!("{name}.{kind}.stamp"))
}

/// Maximum amount the cached stamp may exceed `now` before we treat
/// the stamp as suspicious and refuse to consider it fresh.
///
/// A small slack (5 minutes) absorbs normal NTP corrections and the
/// occasional VM clock adjustment. Anything beyond that almost
/// certainly indicates a real clock-backward event or a tampered/copied
/// stamp file; in either case the right behavior is to re-do the
/// remote check rather than trust an indefinitely-future-dated stamp.
const FUTURE_STAMP_TOLERANCE_SECS: u64 = 300;

/// Returns whether the stamp is fresh under the supplied runtime flags.
#[must_use]
pub fn remote_fresh(path: &Path, freshness: Freshness) -> bool {
    if freshness.force || freshness.reinstall {
        return false;
    }

    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(cached) = content.trim_end().parse::<u64>() else {
        return false;
    };

    // A stamp materially in the future of `now` means the wall clock
    // moved backward (NTP correction, VM suspend/resume, backup
    // restore). Without this guard, `saturating_sub` would return 0
    // for any future-dated stamp and treat the dep as fresh forever
    // until the wall clock caught back up to `cached + ttl` — which
    // could be hours or days, during which no remote checks run.
    if cached > freshness.now.saturating_add(FUTURE_STAMP_TOLERANCE_SECS) {
        return false;
    }

    // `saturating_sub` avoids an underflow if `cached` is within
    // tolerance but still slightly ahead of `now`. The tolerance
    // window above guarantees we only reach this branch when the
    // delta is tiny, so treating "near-future" as fresh remains
    // safe and matches the Bash arithmetic intent.
    freshness.now.saturating_sub(cached) < freshness.ttl
}

/// Writes a remote-check stamp using epoch seconds.
pub fn remote_touch(path: &Path, now: u64) -> Result<()> {
    state::write_atomic(path, &format!("{now}\n"))
}

/// Returns whether a remote stamp was written for this update's timestamp.
///
/// This is deliberately narrower than `remote_fresh`: force/reinstall should
/// still bypass old TTL state, but two phases inside the same `shdeps update`
/// may share a remote fact that was fetched seconds earlier in the same run.
#[must_use]
pub fn remote_checked_at(path: &Path, now: u64) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.trim_end().parse::<u64>().ok() == Some(now)
}

/// Returns a dependency's cached git revision stamp path.
#[must_use]
pub fn revision_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.rev"))
}

/// Reads a cached git revision. Missing revision stamps are normal cache misses.
pub fn revision_read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content.trim_end().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Writes a cached git revision.
pub fn revision_touch(path: &Path, revision: &str) -> Result<()> {
    state::write_atomic(path, &format!("{revision}\n"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        Freshness, remote_fresh, remote_path, remote_touch, revision_path, revision_read,
        revision_touch,
    };

    #[test]
    fn remote_stamp_path_includes_name_and_kind() {
        assert_eq!(
            remote_path(PathBuf::from("/tmp/state").as_path(), "test-tool", "repo"),
            PathBuf::from("/tmp/state/test-tool.repo.stamp")
        );
    }

    #[test]
    fn remote_touch_writes_epoch_seconds() {
        let dir = temp_dir("remote-touch");
        let stamp = remote_path(&dir, "test-tool", "repo");

        remote_touch(&stamp, 1_700_000_000).unwrap();

        assert_eq!(fs::read_to_string(stamp).unwrap(), "1700000000\n");
    }

    #[test]
    fn remote_fresh_respects_ttl() {
        let dir = temp_dir("remote-fresh");
        let stamp = remote_path(&dir, "test-tool", "repo");
        remote_touch(&stamp, 1_700_000_000).unwrap();

        assert!(remote_fresh(
            &stamp,
            Freshness {
                now: 1_700_000_100,
                ttl: 3600,
                force: false,
                reinstall: false,
            }
        ));
        assert!(!remote_fresh(
            &stamp,
            Freshness {
                now: 1_700_010_000,
                ttl: 3600,
                force: false,
                reinstall: false,
            }
        ));
    }

    #[test]
    fn remote_fresh_rejects_stamp_materially_in_the_future() {
        // A stamp that is dated significantly past `now` indicates a
        // clock-backward event (NTP correction, VM resume, backup
        // restore). Without this guard the dep would be treated as
        // fresh until the wall clock caught back up, which could be
        // hours or days — silently skipping all remote checks.
        let dir = temp_dir("remote-future");
        let stamp = remote_path(&dir, "test-tool", "repo");
        remote_touch(&stamp, 2_000_000_000).unwrap();

        assert!(
            !remote_fresh(
                &stamp,
                Freshness {
                    now: 1_700_000_000,
                    ttl: 3600,
                    force: false,
                    reinstall: false,
                }
            ),
            "a stamp dated far in the future must NOT count as fresh"
        );
    }

    #[test]
    fn remote_fresh_tolerates_small_clock_skew() {
        // Real systems experience small NTP-driven clock adjustments
        // (a few seconds, occasionally a minute). The tolerance window
        // (5 minutes) keeps such normal skew from forcing spurious
        // remote checks — only stamps materially in the future fail.
        let dir = temp_dir("remote-small-skew");
        let stamp = remote_path(&dir, "test-tool", "repo");
        remote_touch(&stamp, 1_700_000_120).unwrap();

        assert!(
            remote_fresh(
                &stamp,
                Freshness {
                    // `cached` is 2 minutes ahead of `now` — within
                    // the 5-minute tolerance, so still considered fresh.
                    now: 1_700_000_000,
                    ttl: 3600,
                    force: false,
                    reinstall: false,
                }
            ),
            "small clock skew within tolerance must still be fresh"
        );
    }

    #[test]
    fn remote_fresh_bypasses_for_force_and_reinstall() {
        let dir = temp_dir("remote-bypass");
        let stamp = remote_path(&dir, "test-tool", "repo");
        remote_touch(&stamp, 1_700_000_000).unwrap();

        assert!(!remote_fresh(
            &stamp,
            Freshness {
                now: 1_700_000_001,
                ttl: 3600,
                force: true,
                reinstall: false,
            }
        ));
        assert!(!remote_fresh(
            &stamp,
            Freshness {
                now: 1_700_000_001,
                ttl: 3600,
                force: false,
                reinstall: true,
            }
        ));
    }

    #[test]
    fn remote_fresh_treats_missing_or_invalid_stamp_as_stale() {
        let dir = temp_dir("remote-invalid");
        let stamp = remote_path(&dir, "test-tool", "repo");

        assert!(!remote_fresh(&stamp, freshness()));
        fs::write(&stamp, "not-a-number\n").unwrap();
        assert!(!remote_fresh(&stamp, freshness()));
    }

    #[test]
    fn revision_stamp_path_and_round_trip() {
        let dir = temp_dir("revision");
        let stamp = revision_path(&dir, "test-rev");

        assert_eq!(stamp, dir.join("test-rev.rev"));
        assert_eq!(revision_read(&stamp).unwrap(), None);

        revision_touch(&stamp, "abc123def").unwrap();
        assert_eq!(revision_read(&stamp).unwrap().as_deref(), Some("abc123def"));
    }

    fn freshness() -> Freshness {
        Freshness {
            now: 1_700_000_001,
            ttl: 3600,
            force: false,
            reinstall: false,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-stamp-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
