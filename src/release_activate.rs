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
                return Err(Failure::Rollback { switch, rollback });
            }
        }
        return Err(Failure::Switch(switch));
    }

    if had_existing {
        // The live install has already switched successfully. Backup cleanup is
        // best-effort so an antivirus, shell cwd, or transient file handle does
        // not turn a successful self-update into a rollback of a good install.
        let _ = fs::remove_dir_all(&backup);
    }

    Ok(Activation {
        install_dir: install_dir.to_path_buf(),
    })
}

fn backup_path(install_dir: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    install_dir.with_extension(format!("shdeps-backup-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{activate, Failure};
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
        assert!(fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("backup")));
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
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-release-activate-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
