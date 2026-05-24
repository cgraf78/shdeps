//! `shdeps self-update` planning and source-checkout behavior.
//!
//! Release-archive self-update has stricter rollback and checksum requirements,
//! so this module starts with the behavior the Bash implementation already owns:
//! source checkouts. Keeping it separate from CLI dispatch lets the eventual
//! release-install path share one summary type and one "do not break the
//! existing install" contract.

use std::path::{Path, PathBuf};

use crate::install_metadata::{self, Metadata, Method, Read};
use crate::process::Runner;
use crate::Result;

/// Self-update target selected from checkout shape and install metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The install is a git checkout and should use source-checkout behavior.
    SourceCheckout,
    /// The install metadata identifies a release archive install.
    ReleaseArchive(Metadata),
    /// The install cannot be updated safely by this implementation.
    Unsupported {
        /// Clear diagnostic explaining why automatic self-update is unsafe.
        reason: String,
    },
}

/// Outcome of a self-update attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The target is not a supported self-updatable install.
    Unsupported,
    /// The checkout is dirty, so it was intentionally left untouched.
    DirtySkipped,
    /// A clean checkout ran `git pull --ff-only --quiet` successfully.
    Pulled,
    /// The pull failed non-destructively; the existing checkout remains usable.
    PullFailed,
}

/// Summary returned by self-update code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Directory that was inspected.
    pub dir: PathBuf,
    /// Final outcome.
    pub outcome: Outcome,
    /// True when the caller should re-link shdeps-owned extras from `dir`.
    pub relink_extras: bool,
}

impl Summary {
    /// Returns the command's compatibility exit code for this outcome.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self.outcome {
            Outcome::Unsupported => 1,
            Outcome::DirtySkipped | Outcome::Pulled | Outcome::PullFailed => 0,
        }
    }
}

/// Selects the self-update mode for an install directory.
pub fn target(dir: &Path) -> Result<Target> {
    if dir.join(".git").is_dir() {
        // A real checkout wins over metadata because dirty-tree preservation is
        // an active development safety rule. A developer checkout may have
        // stale metadata from conversion experiments; treating `.git` as
        // authoritative avoids accidentally replacing a working tree with a
        // release archive.
        return Ok(Target::SourceCheckout);
    }

    match install_metadata::read(dir)? {
        Read::Missing => Ok(Target::Unsupported {
            reason: "shdeps: not a git clone and no install metadata; rerun install.sh".to_owned(),
        }),
        Read::Invalid { reason } => Ok(Target::Unsupported {
            reason: format!("shdeps: invalid install metadata: {reason}; rerun install.sh"),
        }),
        Read::Valid(metadata) => match metadata.method {
            Method::Release => Ok(Target::ReleaseArchive(metadata)),
            Method::Git => Ok(Target::Unsupported {
                reason: "shdeps: install metadata says git but .git is missing; rerun install.sh"
                    .to_owned(),
            }),
            Method::SourceBuild | Method::Manual => Ok(Target::Unsupported {
                reason: format!(
                    "shdeps: {} installs are not self-updatable; rerun install.sh",
                    metadata.method
                ),
            }),
        },
    }
}

/// Runs the source-checkout portion of `shdeps self-update`.
pub fn source_checkout(dir: &Path, runner: &impl Runner) -> Result<Summary> {
    if !dir.join(".git").is_dir() {
        return Ok(Summary {
            dir: dir.to_path_buf(),
            outcome: Outcome::Unsupported,
            relink_extras: false,
        });
    }

    let status = runner.run(
        "git",
        &[
            "-C",
            &dir.display().to_string(),
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ],
        None,
    )?;
    if !status.stdout.is_empty() {
        return Ok(Summary {
            dir: dir.to_path_buf(),
            outcome: Outcome::DirtySkipped,
            relink_extras: true,
        });
    }

    let pull = runner.run(
        "git",
        &[
            "-C",
            &dir.display().to_string(),
            "pull",
            "--ff-only",
            "--quiet",
        ],
        None,
    )?;
    Ok(Summary {
        dir: dir.to_path_buf(),
        outcome: if pull.success {
            Outcome::Pulled
        } else {
            // Bash warns but returns success after a failed pull because the
            // existing checkout is still the active, usable implementation.
            // Preserve that non-destructive bias; release-mode rollback will
            // use stricter failure handling when it lands.
            Outcome::PullFailed
        },
        relink_extras: true,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{source_checkout, target, Outcome, Target};
    use crate::install_metadata::{write, Metadata, Method};
    use crate::process::{Output, Runner};

    #[derive(Debug, Default)]
    struct FakeRunner {
        outputs: BTreeMap<(String, Vec<String>), Output>,
    }

    impl FakeRunner {
        fn with_output<const N: usize>(
            mut self,
            program: &str,
            args: [&str; N],
            success: bool,
            stdout: &str,
        ) -> Self {
            self.outputs.insert(
                (
                    program.to_owned(),
                    args.into_iter().map(str::to_owned).collect(),
                ),
                Output {
                    success,
                    timed_out: false,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                },
            );
            self
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, _command: &str) -> bool {
            true
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            self.outputs
                .get(&(
                    program.to_owned(),
                    args.iter().copied().map(str::to_owned).collect(),
                ))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
        }
    }

    #[test]
    fn unsupported_when_directory_is_not_git_checkout() {
        let dir = temp_dir("unsupported");

        let summary = source_checkout(&dir, &FakeRunner::default()).unwrap();

        assert_eq!(summary.outcome, Outcome::Unsupported);
        assert_eq!(summary.exit_code(), 1);
        assert!(!summary.relink_extras);
    }

    #[test]
    fn dirty_checkout_skips_pull_but_still_relinks_extras() {
        let dir = checkout("dirty");
        let runner = FakeRunner::default().with_output(
            "git",
            [
                "-C",
                &dir.display().to_string(),
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
            true,
            " M shdeps.sh\n",
        );

        let summary = source_checkout(&dir, &runner).unwrap();

        assert_eq!(summary.outcome, Outcome::DirtySkipped);
        assert_eq!(summary.exit_code(), 0);
        assert!(summary.relink_extras);
    }

    #[test]
    fn clean_checkout_pulls_with_ff_only_quiet() {
        let dir = checkout("clean");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                true,
                "",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
                true,
                "",
            );

        let summary = source_checkout(&dir, &runner).unwrap();

        assert_eq!(summary.outcome, Outcome::Pulled);
        assert!(summary.relink_extras);
    }

    #[test]
    fn pull_failure_is_non_destructive_success_outcome() {
        let dir = checkout("pull-fails");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                true,
                "",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
                false,
                "",
            );

        let summary = source_checkout(&dir, &runner).unwrap();

        assert_eq!(summary.outcome, Outcome::PullFailed);
        assert_eq!(summary.exit_code(), 0);
        assert!(summary.relink_extras);
    }

    #[test]
    fn target_prefers_git_checkout_over_metadata() {
        let dir = checkout("target-git");
        write(&dir, &Metadata::new(Method::Release)).unwrap();

        assert_eq!(target(&dir).unwrap(), Target::SourceCheckout);
    }

    #[test]
    fn target_uses_release_metadata_for_archive_installs() {
        let dir = temp_dir("target-release");
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.23".to_owned());
        write(&dir, &metadata).unwrap();

        assert_eq!(target(&dir).unwrap(), Target::ReleaseArchive(metadata));
    }

    #[test]
    fn target_reports_missing_and_invalid_metadata_clearly() {
        let missing = target(&temp_dir("target-missing")).unwrap();
        assert!(
            matches!(missing, Target::Unsupported { reason } if reason.contains("no install metadata"))
        );

        let invalid = temp_dir("target-invalid");
        fs::write(
            invalid.join(".shdeps-install.json"),
            r#"{"schema":2,"method":"release"}"#,
        )
        .unwrap();
        let invalid_target = target(&invalid).unwrap();
        assert!(
            matches!(invalid_target, Target::Unsupported { reason } if reason.contains("invalid install metadata"))
        );
    }

    #[test]
    fn target_rejects_metadata_methods_without_safe_update_path() {
        let git = temp_dir("target-metadata-git");
        write(&git, &Metadata::new(Method::Git)).unwrap();
        let git_target = target(&git).unwrap();
        assert!(
            matches!(git_target, Target::Unsupported { reason } if reason.contains(".git is missing"))
        );

        let manual = temp_dir("target-manual");
        write(&manual, &Metadata::new(Method::Manual)).unwrap();
        let manual_target = target(&manual).unwrap();
        assert!(
            matches!(manual_target, Target::Unsupported { reason } if reason.contains("manual installs are not self-updatable"))
        );
    }

    fn checkout(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-self-update-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
