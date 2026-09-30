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

fn path(state_dir: &Path, kind: Kind) -> PathBuf {
    state_dir.join(match kind {
        Kind::Posts => ".deferred-posts",
        Kind::Uninstalls => ".deferred-uninstalls",
    })
}

fn read_lines(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .map(|content| content.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Records this run's deferred set and reports whether to announce it.
///
/// An empty set clears the record, so a deferral that clears and later recurs
/// is announced again. With a terminal the answer is always "announce". The
/// record is advisory: if it cannot be read or written, announce rather than
/// risk hiding a new deferral.
pub(crate) fn record(state_dir: &Path, kind: Kind, names: &[String], terminal: bool) -> bool {
    let path = path(state_dir, kind);
    let current = names
        .iter()
        .map(|name| format!("{name}|{REASON_SUDO_NO_TERMINAL}"))
        .collect::<BTreeSet<_>>();
    if current.is_empty() {
        // Best effort: a record that cannot be removed only delays the next
        // announcement of the same set, and there is nothing to announce now.
        let _ = std::fs::remove_file(&path);
        return false;
    }
    let previous = read_lines(&path);
    let new_entry = !current.is_subset(&previous);
    if current != previous {
        let mut content = current.iter().cloned().collect::<Vec<_>>().join("\n");
        content.push('\n');
        if crate::state::write_atomic(&path, &content).is_err() {
            return true;
        }
    }
    terminal || new_entry
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
    use super::{Kind, record, recorded};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn announces_new_entries_only_and_again_after_clearing() {
        let state = crate::test_support::temp_dir("shdeps-deferral-notice");
        std::fs::create_dir_all(&state).unwrap();

        assert!(record(&state, Kind::Uninstalls, &names(&["a"]), false));
        assert!(!record(&state, Kind::Uninstalls, &names(&["a"]), false));
        assert!(record(&state, Kind::Uninstalls, &names(&["a", "b"]), false));
        assert!(!record(&state, Kind::Uninstalls, &names(&["b"]), false));
        assert_eq!(recorded(&state, Kind::Uninstalls), ["b".to_owned()].into());
        assert!(record(&state, Kind::Uninstalls, &names(&["b"]), true));
        assert!(!record(&state, Kind::Uninstalls, &[], false));
        assert!(recorded(&state, Kind::Uninstalls).is_empty());
        assert!(record(&state, Kind::Uninstalls, &names(&["b"]), false));
        assert!(
            recorded(&state, Kind::Posts).is_empty(),
            "kinds are independent"
        );
    }
}
