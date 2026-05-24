//! Remote TTL and git revision stamp files.
//!
//! Stamps are deliberately tiny text files because they sit on the no-op
//! update path. A fresh stamp lets `shdeps` skip network/API work; force and
//! reinstall modes always bypass freshness so explicit user intent wins over
//! cached state.

use std::fs;
use std::path::{Path, PathBuf};

use crate::state;
use crate::Result;

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

    // `saturating_sub` avoids an underflow if the system clock moves backward.
    // Treating a future stamp as fresh mirrors the Bash arithmetic intent more
    // closely than turning clock skew into a forced network check.
    freshness.now.saturating_sub(cached) < freshness.ttl
}

/// Writes a remote-check stamp using epoch seconds.
pub fn remote_touch(path: &Path, now: u64) -> Result<()> {
    state::write_atomic(path, &format!("{now}\n"))
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
        remote_fresh, remote_path, remote_touch, revision_path, revision_read, revision_touch,
        Freshness,
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
