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

/// Release metadata needed for self-update selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Release tag name from the GitHub API.
    pub tag: String,
    /// True when GitHub marks the release as a draft.
    pub draft: bool,
    /// True when GitHub marks the release as a prerelease.
    pub prerelease: bool,
}

/// Decision after applying release-selection rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseDecision {
    /// A release should be downloaded and verified.
    Update(Release),
    /// No non-draft, non-prerelease release is available.
    NoSelectableRelease,
    /// The selectable release is not newer than the installed tag.
    NoUpdate {
        /// Installed tag from metadata.
        current: String,
        /// Best selectable tag that was considered.
        candidate: String,
    },
}

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

/// Selects the release archive candidate for a metadata-backed self-update.
#[must_use]
pub fn select_release(releases: &[Release], installed_tag: Option<&str>) -> ReleaseDecision {
    // The Bash implementation only ever consumed stable release artifacts.
    // Keeping draft/prerelease filtering here prevents a later download layer
    // from accidentally treating GitHub's newest experimental build as the
    // drop-in replacement that fleet auto-update expects to be boring.
    let Some(candidate) = releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .max_by(|left, right| compare_tags(&left.tag, &right.tag))
        .cloned()
    else {
        return ReleaseDecision::NoSelectableRelease;
    };

    if let Some(current) = installed_tag.filter(|tag| !tag.is_empty()) {
        // Metadata is the only durable proof of which archive was installed.
        // If the best selectable release does not compare newer, refuse to
        // rewrite files: self-update should be monotonic unless the user does
        // an explicit reinstall or manual rollback.
        if compare_tags(&candidate.tag, current).is_le() {
            return ReleaseDecision::NoUpdate {
                current: current.to_owned(),
                candidate: candidate.tag,
            };
        }
    }

    ReleaseDecision::Update(candidate)
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

fn compare_tags(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_parts = TagParts::new(left);
    let mut right_parts = TagParts::new(right);

    loop {
        match (left_parts.next(), right_parts.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = left.cmp(&right);
                if !ordering.is_eq() {
                    return ordering;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagPart<'a> {
    Number(&'a str),
    Text(&'a str),
}

impl TagPart<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => cmp_number(left, right),
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            // Numeric chunks sort after text chunks so `v10` compares greater
            // than `vbeta`. This is intentionally only a stable natural sort,
            // not semver: shdeps runtime versions are commit/release-label
            // based, and the spec only needs a deterministic downgrade guard.
            (Self::Number(_), Self::Text(_)) => std::cmp::Ordering::Greater,
            (Self::Text(_), Self::Number(_)) => std::cmp::Ordering::Less,
        }
    }
}

struct TagParts<'a> {
    value: &'a str,
}

impl<'a> TagParts<'a> {
    fn new(value: &'a str) -> Self {
        Self { value }
    }
}

impl<'a> Iterator for TagParts<'a> {
    type Item = TagPart<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.value;
        if value.is_empty() {
            return None;
        }

        let numeric = value.as_bytes()[0].is_ascii_digit();
        let split = value
            .char_indices()
            .find(|(_, ch)| ch.is_ascii_digit() != numeric)
            .map(|(index, _)| index)
            .unwrap_or(value.len());
        let (part, rest) = value.split_at(split);
        self.value = rest;

        Some(if numeric {
            TagPart::Number(part)
        } else {
            TagPart::Text(part)
        })
    }
}

fn cmp_number(left: &str, right: &str) -> std::cmp::Ordering {
    // Compare by normalized length before bytes so arbitrarily large date-ish
    // or build-number tags never need to fit in a Rust integer type.
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    let left = if left.is_empty() { "0" } else { left };
    let right = if right.is_empty() { "0" } else { right };

    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        select_release, source_checkout, target, Outcome, Release, ReleaseDecision, Target,
    };
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

    #[test]
    fn release_selection_skips_drafts_and_prereleases() {
        let decision = select_release(
            &[
                release("v2026.05.24", true, false),
                release("v2026.05.25", false, true),
                release("v2026.05.23", false, false),
            ],
            Some("v2026.05.22"),
        );

        assert_eq!(
            decision,
            ReleaseDecision::Update(release("v2026.05.23", false, false))
        );
    }

    #[test]
    fn release_selection_reports_no_selectable_release() {
        let decision = select_release(
            &[
                release("v2026.05.24", true, false),
                release("v2026.05.25", false, true),
            ],
            Some("v2026.05.22"),
        );

        assert_eq!(decision, ReleaseDecision::NoSelectableRelease);
    }

    #[test]
    fn release_selection_refuses_equal_or_older_tags() {
        assert_eq!(
            select_release(&[release("v2026.05.22", false, false)], Some("v2026.05.23")),
            ReleaseDecision::NoUpdate {
                current: "v2026.05.23".to_owned(),
                candidate: "v2026.05.22".to_owned(),
            }
        );
        assert_eq!(
            select_release(&[release("v2026.05.23", false, false)], Some("v2026.05.23")),
            ReleaseDecision::NoUpdate {
                current: "v2026.05.23".to_owned(),
                candidate: "v2026.05.23".to_owned(),
            }
        );
    }

    #[test]
    fn release_selection_uses_natural_tag_order_not_plain_lexical_order() {
        let decision = select_release(
            &[
                release("v2026.05.9", false, false),
                release("v2026.05.10", false, false),
            ],
            Some("v2026.05.8"),
        );

        assert_eq!(
            decision,
            ReleaseDecision::Update(release("v2026.05.10", false, false))
        );
    }

    #[test]
    fn release_selection_allows_update_when_current_tag_is_unknown() {
        let decision = select_release(&[release("v2026.05.23", false, false)], None);

        assert_eq!(
            decision,
            ReleaseDecision::Update(release("v2026.05.23", false, false))
        );
    }

    fn checkout(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
        Release {
            tag: tag.to_owned(),
            draft,
            prerelease,
        }
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
