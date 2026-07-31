//! Transactional activation for staged shdeps release installs.
//!
//! Activation is intentionally separate from staging. By the time this module
//! runs, the candidate archive has already been downloaded, checksummed,
//! extracted, and validated. The remaining job is to switch directories without
//! stranding the user on a half-written install, which is why rollback lives in
//! one place rather than in the download or extraction helpers.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::install_metadata::{self, Metadata};
use crate::release_stage::Staged;

/// Result of activating a staged release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// Directory now serving as the live shdeps install.
    pub install_dir: PathBuf,
}

/// Failure while switching a staged release into the live install path.
#[derive(Debug)]
pub enum Failure {
    /// Metadata could not be written into the staged tree before activation.
    Metadata(io::Error),
    /// Existing install could not be moved aside.
    Backup(io::Error),
    /// Staged install could not be moved into place.
    Switch(io::Error),
    /// Rollback failed after the live install had been moved aside.
    Rollback {
        /// Original switch error.
        switch: io::Error,
        /// Error from restoring the previous install.
        rollback: io::Error,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => write!(formatter, "metadata write failed: {error}"),
            Self::Backup(error) => write!(formatter, "backup failed: {error}"),
            Self::Switch(error) => write!(formatter, "activation failed: {error}"),
            Self::Rollback { switch, rollback } => {
                write!(
                    formatter,
                    "activation failed ({switch}); rollback failed ({rollback})"
                )
            }
        }
    }
}

impl std::error::Error for Failure {}

/// Activates `staged` at `install_dir`, preserving the old install on failure.
pub fn activate(
    staged: &Staged,
    install_dir: &Path,
    metadata: &Metadata,
) -> Result<Activation, Failure> {
    if !staged.dir.is_dir() {
        return Err(Failure::Metadata(io::Error::new(
            io::ErrorKind::NotFound,
            "staged install directory is missing",
        )));
    }

    install_metadata::write(&staged.dir, metadata)
        .map_err(|error| Failure::Metadata(io::Error::other(error.to_string())))?;

    let backup = backup_path(install_dir);
    let had_existing = install_dir.exists();
    if had_existing {
        fs::rename(install_dir, &backup).map_err(Failure::Backup)?;
    }

    if let Err(switch) = fs::rename(&staged.dir, install_dir) {
        if had_existing {
            if let Err(rollback) = fs::rename(&backup, install_dir) {
                // Two failures in a row: the rename into the live path
                // failed, then the restore from backup also failed.
                // The install state is genuinely ambiguous, so we keep
                // both `staged.dir` and `backup` on disk for the user
                // to inspect rather than discarding evidence with a
                // best-effort cleanup. Returning the combined error
                // is the operator's signal to investigate manually.
                return Err(Failure::Rollback { switch, rollback });
            }
        }
        // The switch failed but the previous install (if any) has been
        // successfully restored. The staged candidate is now orphaned
        // — leaving it on disk would let it accumulate on every retry,
        // especially under disk-full conditions where each new
        // `self-update` would stack another `.shdeps-stage-...`
        // directory next to the live install. Best-effort cleanup
        // here keeps the directory listing tidy without masking the
        // underlying switch error.
        let _ = remove_path(&staged.dir);
        return Err(Failure::Switch(switch));
    }

    if had_existing {
        // The live install has already switched successfully. Backup
        // cleanup is best-effort so an antivirus, shell cwd, or
        // transient file handle does not turn a successful self-
        // update into a rollback of a good install. Route through
        // `remove_path` (which checks `symlink_metadata` first) so
        // that if `install_dir` was a symlink to a real directory —
        // unusual but legal — the backup we just renamed is a
        // symlink whose target must NOT be touched. `remove_dir_all`
        // on a symlinked dir has uncertain semantics across Rust
        // stdlib versions; `remove_path` is unambiguous.
        let _ = remove_path(&backup);
    }

    Ok(Activation {
        install_dir: install_dir.to_path_buf(),
    })
}

/// Removes the entry at `path` without following symlinks.
///
/// Mirrors the local `remove_any` helpers in `github_release_install`
/// and `update_transition`. The three callers are now identical, and
/// the helper is small enough to keep duplicated rather than introduce
/// a `fs_util` module just for this — but the consolidation point is
/// worth noting for a future "fourth use → extract" decision.
fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn backup_path(install_dir: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let suffix = format!(".shdeps-backup-{}-{nanos}", std::process::id());

    // Append the suffix to the full final component rather than using
    // `with_extension`, which REPLACES any existing extension: an
    // install dir like `shdeps.dev` would otherwise back up to
    // `shdeps.shdeps-backup-...`, silently dropping `.dev`. The backup is
    // renamed back to the original `install_dir` on rollback, so a wrong
    // name is not corrupting, but it should still visibly correspond to
    // its source. Fall back to `with_extension` only for the degenerate
    // case of a path with no final component (e.g. `/`).
    match install_dir.file_name() {
        Some(name) => {
            let mut file_name = name.to_os_string();
            file_name.push(&suffix);
            install_dir.with_file_name(file_name)
        }
        None => install_dir.with_extension(&suffix[1..]),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{Failure, activate};
    use crate::install_metadata::{self, Metadata, Method};
    use crate::release_stage::Staged;

    #[test]
    fn activate_replaces_existing_install_and_writes_metadata() {
        let root = temp_dir("replace");
        let install = root.join("shdeps");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("old"), "old").unwrap();
        let staged = staged(&root, "v2026.05.24");
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.24".to_owned());

        let activation = activate(&staged, &install, &metadata).unwrap();

        assert_eq!(activation.install_dir, install);
        assert_eq!(
            fs::read_to_string(root.join("shdeps/shdeps")).unwrap(),
            "new"
        );
        assert!(!root.join("shdeps/old").exists());
        assert_eq!(
            install_metadata::read(&root.join("shdeps")).unwrap(),
            install_metadata::Read::Valid(metadata)
        );
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("backup")
        }));
    }

    #[test]
    fn activate_installs_when_no_previous_install_exists() {
        let root = temp_dir("fresh");
        let install = root.join("shdeps");
        let staged = staged(&root, "v2026.05.24");
        let metadata = Metadata::new(Method::Release);

        activate(&staged, &install, &metadata).unwrap();

        assert_eq!(fs::read_to_string(install.join("shdeps")).unwrap(), "new");
    }

    #[test]
    fn activate_cleans_up_staged_dir_after_recoverable_switch_failure() {
        // The Switch failure path used to leave `staged.dir` on disk
        // after restoring the previous install. Under disk-full
        // conditions the directory would accumulate on every retry,
        // wasting inodes and confusing later cleanup. The fix removes
        // the orphaned staged dir best-effort.
        //
        // Force a Switch failure by passing an `install_dir` whose
        // parent does not exist — `rename` cannot create the parent,
        // so the staged-to-live rename fails reliably regardless of
        // the underlying filesystem.
        let root = temp_dir("switch-cleanup");
        let install = root.join("missing-parent/shdeps");
        let staged = staged(&root, "v2026.05.24");
        let staged_dir = staged.dir.clone();

        let failure = activate(&staged, &install, &Metadata::new(Method::Release)).unwrap_err();

        assert!(
            matches!(failure, Failure::Switch(_)),
            "switch failure expected, got {failure:?}"
        );
        assert!(
            !staged_dir.exists(),
            "orphaned staged dir must be cleaned up after a recoverable switch failure"
        );
    }

    #[test]
    fn activate_fails_before_live_switch_when_metadata_cannot_be_written() {
        let root = temp_dir("metadata");
        let install = root.join("shdeps");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("old"), "old").unwrap();
        let staged = Staged {
            dir: root.join("missing-stage"),
            tag: "v2026.05.24".to_owned(),
        };

        let failure = activate(&staged, &install, &Metadata::new(Method::Release)).unwrap_err();

        assert!(matches!(failure, Failure::Metadata(_)));
        assert_eq!(fs::read_to_string(install.join("old")).unwrap(), "old");
    }

    #[test]
    fn backup_path_preserves_dotted_install_dir_suffix() {
        // `with_extension` would turn `shdeps.dev` into
        // `shdeps.shdeps-backup-...`, dropping `.dev`. The backup name must
        // keep the full original final component so it visibly corresponds to
        // its source directory.
        let backup = super::backup_path(std::path::Path::new("/tmp/tools/shdeps.dev"));
        let name = backup.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("shdeps.dev.shdeps-backup-"),
            "backup name must preserve the dotted suffix: {name}"
        );
        assert_eq!(backup.parent().unwrap(), std::path::Path::new("/tmp/tools"));
    }

    #[test]
    fn backup_path_appends_suffix_to_extensionless_install_dir() {
        // The canonical install dir has no extension; the backup is the
        // original name plus the marker suffix.
        let backup = super::backup_path(std::path::Path::new("/tmp/share/shdeps"));
        let name = backup.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("shdeps.shdeps-backup-"),
            "backup name must append the marker suffix: {name}"
        );
    }

    fn staged(root: &std::path::Path, tag: &str) -> Staged {
        let dir = root.join(format!("stage-{tag}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("shdeps"), "new").unwrap();
        Staged {
            dir,
            tag: tag.to_owned(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-release-activate-{name}"))
    }
}
