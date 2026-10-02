//! Once-per-state notices for hooks deferred until sudo can prompt.
//!
//! A hook that needs sudo is deferred, not failed, when Shdeps has no
//! controlling terminal (see `update::SUDO_NO_TERMINAL`). An unattended caller
//! such as cron then hits the same deferral every run; repeating the warning on
//! stderr each time would mail the owner dozens of times a day about unchanged,
//! expected state. This module records the deferred set in the state directory
//! so terminal-less runs announce it only when it gains a new entry, while
//! terminal runs always announce and `prune --dry-run` can still show it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Which command's deferrals a record tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `post()` hooks deferred by `shdeps update`.
    Posts,
    /// `uninstall()` hooks deferred by `shdeps prune`.
    Uninstalls,
}

/// Why the recorded dependencies are deferred. Stored with each name so a
/// future reason is a new state (and announced) rather than a silent repeat.
const REASON_SUDO_NO_TERMINAL: &str = "sudo-no-terminal";

/// Returns the record file for one deferral kind.
pub(crate) fn path(state_dir: &Path, kind: Kind) -> PathBuf {
    state_dir.join(match kind {
        Kind::Posts => ".deferred-posts",
        Kind::Uninstalls => ".deferred-uninstalls",
    })
}

fn read_lines(path: &Path) -> BTreeSet<String> {
    crate::state::read_regular_to_string(path)
        .map(|content| content.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// How a run may change the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The run prints the warning itself; `terminal` means it always does.
    Announce {
        /// Whether a controlling terminal (an attentive user) is present.
        terminal: bool,
    },
    /// A consumer (JSONL) decides whether anyone sees the warning, so this
    /// run may retire entries that are no longer deferred but must not mark
    /// new ones as announced.
    ShrinkOnly,
}

/// Records this run's deferred set and reports whether to announce it.
///
/// Each entry is a dependency name plus the hook's own failure detail (empty
/// when none); a changed detail is new state, because a deferral cannot prove
/// sudo caused the failure and must not hide a different one. An empty set
/// clears the record, so a deferral that clears and later recurs is announced
/// again. The record is advisory and written after the state lock is
/// released, so racing runs may cost one extra or one missed repeat until the
/// next run rewrites it; unreadable or unwritable records err toward
/// announcing.
pub(crate) fn record(state_dir: &Path, kind: Kind, entries: &[(&str, &str)], mode: Mode) -> bool {
    let path = path(state_dir, kind);
    let current = entries
        .iter()
        .map(|(name, detail)| format!("{name}|{REASON_SUDO_NO_TERMINAL}|{detail}"))
        .collect::<BTreeSet<_>>();
    let previous = read_lines(&path);
    let next = match mode {
        Mode::Announce { .. } => current.clone(),
        Mode::ShrinkOnly => current.intersection(&previous).cloned().collect(),
    };
    if next.is_empty() {
        // Best effort: a record that cannot be removed only delays the next
        // announcement of the same set.
        let _ = std::fs::remove_file(&path);
    } else if next != previous {
        let mut content = next.iter().cloned().collect::<Vec<_>>().join("\n");
        content.push('\n');
        if crate::state::write_atomic(&path, &content).is_err() {
            return true;
        }
    }
    match mode {
        Mode::Announce { terminal } => {
            !current.is_empty() && (terminal || !current.is_subset(&previous))
        }
        Mode::ShrinkOnly => true,
    }
}

/// Returns the recorded deferred names, for on-demand reporting.
pub(crate) fn recorded(state_dir: &Path, kind: Kind) -> BTreeSet<String> {
    read_lines(&path(state_dir, kind))
        .into_iter()
        .filter_map(|line| line.split_once('|').map(|(name, _)| name.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Kind, Mode, record, recorded};

    fn names<'a>(list: &[&'a str]) -> Vec<(&'a str, &'a str)> {
        list.iter().map(|name| (*name, "")).collect()
    }

    const UNATTENDED: Mode = Mode::Announce { terminal: false };

    #[test]
    fn announces_new_entries_only_and_again_after_clearing() {
        let state = crate::test_support::temp_dir("shdeps-deferral-notice");
        std::fs::create_dir_all(&state).unwrap();

        assert!(record(&state, Kind::Uninstalls, &names(&["a"]), UNATTENDED));
        assert!(!record(
            &state,
            Kind::Uninstalls,
            &names(&["a"]),
            UNATTENDED
        ));
        assert!(record(
            &state,
            Kind::Uninstalls,
            &names(&["a", "b"]),
            UNATTENDED
        ));
        assert!(!record(
            &state,
            Kind::Uninstalls,
            &names(&["b"]),
            UNATTENDED
        ));
        assert_eq!(recorded(&state, Kind::Uninstalls), ["b".to_owned()].into());
        assert!(record(
            &state,
            Kind::Uninstalls,
            &names(&["b"]),
            Mode::Announce { terminal: true }
        ));
        assert!(!record(&state, Kind::Uninstalls, &[], UNATTENDED));
        assert!(recorded(&state, Kind::Uninstalls).is_empty());
        assert!(record(&state, Kind::Uninstalls, &names(&["b"]), UNATTENDED));
        assert!(
            recorded(&state, Kind::Posts).is_empty(),
            "kinds are independent"
        );
        assert!(record(
            &state,
            Kind::Uninstalls,
            &[("b", "other")],
            UNATTENDED
        ));

        // A consumer-rendered run never marks entries as announced.
        record(&state, Kind::Posts, &names(&["c"]), Mode::ShrinkOnly);
        assert!(record(&state, Kind::Posts, &names(&["c"]), UNATTENDED));
        record(&state, Kind::Posts, &[], Mode::ShrinkOnly);
        assert!(recorded(&state, Kind::Posts).is_empty());
    }
}
