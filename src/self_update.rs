//! `shdeps self-update` planning and update behavior.
//!
//! Release-archive self-update has stricter rollback and checksum requirements,
//! so this module starts with the behavior the Bash implementation already owns:
//! source checkouts. Keeping it separate from CLI dispatch lets the eventual
//! release-install path share one summary type and one "do not break the
//! existing install" contract.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::github::{self, Release};
use crate::http::Client;
use crate::install_metadata::{self, Metadata, Method, Read};
use crate::process::Runner;
use crate::release_artifact::{self, Pair};
use crate::release_stage;
use crate::runtime::Env;
use crate::Result;

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

/// Decision after selecting a concrete release archive for this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveDecision {
    /// A verified archive/checksum pair can be downloaded and staged.
    Update {
        /// Selected GitHub release.
        release: Release,
        /// Exact archive/checksum assets for the current host.
        pair: Pair,
    },
    /// No non-draft, non-prerelease release is available.
    NoSelectableRelease,
    /// The latest selectable release is not newer than the installed tag.
    NoUpdate {
        /// Installed tag from metadata.
        current: String,
        /// Best selectable tag that was considered.
        candidate: String,
    },
    /// The current host cannot use any published shdeps release artifact.
    UnsupportedPlatform,
    /// The release exists, but not for the current host label.
    NoSupportedArtifact {
        /// Release tag that was selected before artifact matching.
        tag: String,
        /// Exact artifact platform label required for this host.
        platform_label: String,
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
    /// The checkout moved forward, but rebuilding the Rust binary failed.
    BuildFailed,
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

/// Outcome of a release-archive self-update attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseArchiveOutcome {
    /// The release archive was activated.
    Updated {
        /// New active release tag.
        tag: String,
    },
    /// The selected release is not newer than the installed release.
    NoUpdate {
        /// Installed tag from metadata.
        current: String,
        /// Best selectable tag that was considered.
        candidate: String,
    },
    /// No stable release was available.
    NoSelectableRelease,
    /// This host has no supported shdeps release artifact label.
    UnsupportedPlatform,
    /// The selected release did not contain this host's archive/checksum pair.
    NoSupportedArtifact {
        /// Release tag that was selected.
        tag: String,
        /// Required host artifact platform label.
        platform_label: String,
    },
}

/// Summary for release-archive self-update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseArchiveSummary {
    /// Install directory that was inspected and possibly replaced.
    pub dir: PathBuf,
    /// Final release-archive outcome.
    pub outcome: ReleaseArchiveOutcome,
}

impl ReleaseArchiveSummary {
    /// Returns the command's compatibility exit code for this outcome.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self.outcome {
            ReleaseArchiveOutcome::Updated { .. } | ReleaseArchiveOutcome::NoUpdate { .. } => 0,
            ReleaseArchiveOutcome::NoSelectableRelease
            | ReleaseArchiveOutcome::UnsupportedPlatform
            | ReleaseArchiveOutcome::NoSupportedArtifact { .. } => 1,
        }
    }
}

/// Failure after selecting a concrete release archive update.
#[derive(Debug)]
pub enum ReleaseArchiveFailure {
    /// GitHub release metadata could not be fetched or parsed.
    Fetch(crate::Error),
    /// The selected archive failed checksum, extraction, or validation.
    Stage(release_stage::Failure),
    /// The verified staged archive could not be activated safely.
    Activate(crate::release_activate::Failure),
}

impl fmt::Display for ReleaseArchiveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch(error) => write!(formatter, "release metadata fetch failed: {error}"),
            Self::Stage(error) => write!(formatter, "release staging failed: {error}"),
            Self::Activate(error) => write!(formatter, "release activation failed: {error}"),
        }
    }
}

impl std::error::Error for ReleaseArchiveFailure {}

impl Summary {
    /// Returns the command's compatibility exit code for this outcome.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self.outcome {
            Outcome::Unsupported => 1,
            Outcome::DirtySkipped | Outcome::Pulled | Outcome::PullFailed => 0,
            Outcome::BuildFailed => 1,
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

/// Selects the concrete release archive candidate for release-style self-update.
#[must_use]
pub fn select_archive(
    releases: &[Release],
    metadata: &Metadata,
    env: &impl Env,
) -> ArchiveDecision {
    let release = match select_release(releases, metadata.tag.as_deref()) {
        ReleaseDecision::Update(release) => release,
        ReleaseDecision::NoSelectableRelease => return ArchiveDecision::NoSelectableRelease,
        ReleaseDecision::NoUpdate { current, candidate } => {
            return ArchiveDecision::NoUpdate { current, candidate };
        }
    };

    let Some(platform_label) = release_artifact::host_label(env) else {
        return ArchiveDecision::UnsupportedPlatform;
    };

    let Some(pair) = release_artifact::select_pair(&release, &platform_label) else {
        // Do not walk backward to older releases when the latest release is
        // missing this platform. A fallback would silently pin a host behind
        // the rest of the fleet and could hide a broken release workflow.
        return ArchiveDecision::NoSupportedArtifact {
            tag: release.tag,
            platform_label,
        };
    };

    ArchiveDecision::Update { release, pair }
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

    if pull.success && !ensure_source_binary_current(dir, runner)? {
        return Ok(Summary {
            dir: dir.to_path_buf(),
            outcome: Outcome::BuildFailed,
            relink_extras: true,
        });
    }

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

fn ensure_source_binary_current(dir: &Path, runner: &impl Runner) -> Result<bool> {
    if !dir.join("Cargo.toml").is_file() {
        return Ok(true);
    }

    let Some(head) = checkout_head(dir, runner)? else {
        return Ok(true);
    };

    for (relative, link_target) in [
        ("shdeps", None),
        ("target/release/shdeps", Some("target/release/shdeps")),
        ("target/debug/shdeps", Some("target/debug/shdeps")),
    ] {
        if binary_matches_head(&dir.join(relative), &head, runner)? {
            if let Some(link_target) = link_target {
                link_checkout_binary(dir, link_target)?;
            }
            return Ok(true);
        }
    }

    if !runner.exists("cargo") {
        return Ok(false);
    }

    // Source-checkout self-update is the developer/canary path. Release
    // installs update from archives and do not need Rust, but a checked-out
    // Rust tree must keep its sibling binary in lockstep with HEAD or the
    // wrapper can keep executing the previous build after a successful pull.
    let manifest_path = dir.join("Cargo.toml").display().to_string();
    let target_dir = dir.join("target").display().to_string();
    let build = runner.run(
        "cargo",
        &[
            "build",
            "--release",
            "--locked",
            "--manifest-path",
            &manifest_path,
            "--target-dir",
            &target_dir,
        ],
        None,
    )?;
    if !build.success {
        return Ok(false);
    }

    if !binary_matches_head(&dir.join("target/release/shdeps"), &head, runner)? {
        return Ok(false);
    }

    link_checkout_binary(dir, "target/release/shdeps")?;
    Ok(true)
}

fn checkout_head(dir: &Path, runner: &impl Runner) -> Result<Option<String>> {
    let output = runner.run(
        "git",
        &[
            "-C",
            &dir.display().to_string(),
            "rev-parse",
            "--short=8",
            "HEAD",
        ],
        None,
    )?;
    if !output.success {
        return Ok(None);
    }

    let head = output.stdout.trim();
    Ok((!head.is_empty()).then(|| head.to_owned()))
}

fn binary_matches_head(binary: &Path, head: &str, runner: &impl Runner) -> Result<bool> {
    if !crate::process::executable_path(binary) {
        return Ok(false);
    }

    let output = match runner.run(
        &binary.display().to_string(),
        &["version"],
        Some(Duration::from_secs(2)),
    ) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    Ok(output.success && output.stdout.contains(head))
}

fn link_checkout_binary(dir: &Path, target: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let link = dir.join("shdeps");
        match fs::remove_file(&link) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        symlink(target, link)?;
    }

    Ok(())
}

/// Runs the release-archive portion of `shdeps self-update`.
pub fn release_archive(
    dir: &Path,
    metadata: &Metadata,
    env: &impl Env,
    runner: &impl Runner,
    client: &impl Client,
) -> std::result::Result<ReleaseArchiveSummary, ReleaseArchiveFailure> {
    let repo = metadata
        .repo
        .as_deref()
        .filter(|repo| !repo.is_empty())
        .unwrap_or("cgraf78/shdeps");
    let token = github::token(env, runner);
    let releases_json = client
        .get(&github::releases_url(repo), token.as_deref())
        .map_err(crate::Error::from)
        .map_err(ReleaseArchiveFailure::Fetch)?;
    let releases = github::parse_releases(&String::from_utf8_lossy(&releases_json))
        .map_err(ReleaseArchiveFailure::Fetch)?;

    let (release, pair) = match select_archive(&releases, metadata, env) {
        ArchiveDecision::Update { release, pair } => (release, pair),
        ArchiveDecision::NoSelectableRelease => {
            return Ok(ReleaseArchiveSummary {
                dir: dir.to_path_buf(),
                outcome: ReleaseArchiveOutcome::NoSelectableRelease,
            });
        }
        ArchiveDecision::NoUpdate { current, candidate } => {
            return Ok(ReleaseArchiveSummary {
                dir: dir.to_path_buf(),
                outcome: ReleaseArchiveOutcome::NoUpdate { current, candidate },
            });
        }
        ArchiveDecision::UnsupportedPlatform => {
            return Ok(ReleaseArchiveSummary {
                dir: dir.to_path_buf(),
                outcome: ReleaseArchiveOutcome::UnsupportedPlatform,
            });
        }
        ArchiveDecision::NoSupportedArtifact {
            tag,
            platform_label,
        } => {
            return Ok(ReleaseArchiveSummary {
                dir: dir.to_path_buf(),
                outcome: ReleaseArchiveOutcome::NoSupportedArtifact {
                    tag,
                    platform_label,
                },
            });
        }
    };

    let stage_parent = dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let staged = release_stage::stage(&pair, &release.tag, &stage_parent, token.as_deref(), client)
        .map_err(ReleaseArchiveFailure::Stage)?;
    let mut next_metadata = metadata.clone();
    next_metadata.method = Method::Release;
    next_metadata.version = Some(release.tag.clone());
    next_metadata.tag = Some(release.tag.clone());
    next_metadata.repo = Some(repo.to_owned());
    next_metadata.artifact_platform = artifact_platform_from_pair(&pair);

    crate::release_activate::activate(&staged, dir, &next_metadata)
        .map_err(ReleaseArchiveFailure::Activate)?;

    Ok(ReleaseArchiveSummary {
        dir: dir.to_path_buf(),
        outcome: ReleaseArchiveOutcome::Updated { tag: release.tag },
    })
}

fn artifact_platform_from_pair(pair: &Pair) -> Option<String> {
    let body = pair
        .archive_name
        .strip_prefix("shdeps-")?
        .strip_suffix(".tar.gz")?;
    [
        "linux-x86_64-musl",
        "linux-aarch64-musl",
        "macos-x86_64",
        "macos-aarch64",
    ]
    .iter()
    .find(|label| body.ends_with(&format!("-{label}")))
    .map(|label| (*label).to_owned())
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
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, Header};

    use super::{
        release_archive, select_archive, select_release, source_checkout, target, ArchiveDecision,
        Outcome, ReleaseArchiveFailure, ReleaseArchiveOutcome, ReleaseDecision, Target,
    };
    use crate::checksum;
    use crate::github::{Asset, Release};
    use crate::http::Client;
    use crate::install_metadata::{write, Metadata, Method};
    use crate::process::{Output, Runner};
    use crate::release_artifact::Pair;
    use crate::runtime::Env;

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
    fn current_rust_checkout_skips_rebuild_after_pull() {
        let dir = checkout("current-rust");
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"shdeps\"\n").unwrap();
        executable(&dir.join("shdeps"));
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
            )
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "rev-parse",
                    "--short=8",
                    "HEAD",
                ],
                true,
                "deadbeef\n",
            )
            .with_output(
                &dir.join("shdeps").display().to_string(),
                ["version"],
                true,
                "shdeps 20990203-040506-deadbeef\n",
            );

        let summary = source_checkout(&dir, &runner).unwrap();

        assert_eq!(summary.outcome, Outcome::Pulled);
        assert_eq!(summary.exit_code(), 0);
    }

    #[test]
    fn stale_rust_checkout_reports_build_failure_after_pull() {
        let dir = checkout("stale-rust");
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"shdeps\"\n").unwrap();
        executable(&dir.join("shdeps"));
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
            )
            .with_output(
                "git",
                [
                    "-C",
                    &dir.display().to_string(),
                    "rev-parse",
                    "--short=8",
                    "HEAD",
                ],
                true,
                "deadbeef\n",
            )
            .with_output(
                &dir.join("shdeps").display().to_string(),
                ["version"],
                true,
                "shdeps commit cafebabe\n",
            )
            .with_output(
                "cargo",
                [
                    "build",
                    "--release",
                    "--locked",
                    "--manifest-path",
                    &dir.join("Cargo.toml").display().to_string(),
                    "--target-dir",
                    &dir.join("target").display().to_string(),
                ],
                false,
                "",
            );

        let summary = source_checkout(&dir, &runner).unwrap();

        assert_eq!(summary.outcome, Outcome::BuildFailed);
        assert_eq!(summary.exit_code(), 1);
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

    #[test]
    fn archive_selection_returns_update_with_exact_platform_pair() {
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.23".to_owned());
        let env = FakeEnv::new()
            .with_var("SHDEPS_TEST_PLATFORM", "linux")
            .with_command("uname -m", "x86_64");

        let decision = select_archive(
            &[release_with_assets("v2026.05.24", &["linux-x86_64-musl"])],
            &metadata,
            &env,
        );

        assert_eq!(
            decision,
            ArchiveDecision::Update {
                release: release_with_assets("v2026.05.24", &["linux-x86_64-musl"]),
                pair: Pair {
                    archive_name: "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz".to_owned(),
                    archive_url: "https://example/shdeps-v2026.05.24-linux-x86_64-musl.tar.gz"
                        .to_owned(),
                    checksum_name: "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz.sha256".to_owned(),
                    checksum_url:
                        "https://example/shdeps-v2026.05.24-linux-x86_64-musl.tar.gz.sha256"
                            .to_owned(),
                },
            }
        );
    }

    #[test]
    fn archive_selection_preserves_no_update_and_platform_errors() {
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.24".to_owned());
        let linux = FakeEnv::new()
            .with_var("SHDEPS_TEST_PLATFORM", "linux")
            .with_command("uname -m", "x86_64");
        let freebsd = FakeEnv::new()
            .with_var("SHDEPS_TEST_PLATFORM", "freebsd")
            .with_command("uname -m", "x86_64");

        assert_eq!(
            select_archive(
                &[release_with_assets("v2026.05.24", &["linux-x86_64-musl"])],
                &metadata,
                &linux,
            ),
            ArchiveDecision::NoUpdate {
                current: "v2026.05.24".to_owned(),
                candidate: "v2026.05.24".to_owned(),
            }
        );
        metadata.tag = Some("v2026.05.23".to_owned());
        assert_eq!(
            select_archive(
                &[release_with_assets("v2026.05.24", &["linux-x86_64-musl"])],
                &metadata,
                &freebsd,
            ),
            ArchiveDecision::UnsupportedPlatform
        );
    }

    #[test]
    fn archive_selection_reports_missing_artifact_for_latest_release() {
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.23".to_owned());
        let env = FakeEnv::new()
            .with_var("SHDEPS_TEST_PLATFORM", "macos")
            .with_command("uname -m", "arm64");

        let decision = select_archive(
            &[release_with_assets("v2026.05.24", &["linux-x86_64-musl"])],
            &metadata,
            &env,
        );

        assert_eq!(
            decision,
            ArchiveDecision::NoSupportedArtifact {
                tag: "v2026.05.24".to_owned(),
                platform_label: "macos-aarch64".to_owned(),
            }
        );
    }

    #[test]
    fn release_archive_self_update_returns_no_update_without_download() {
        let dir = temp_dir("release-no-update");
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.24".to_owned());
        metadata.repo = Some("cgraf78/shdeps".to_owned());
        let env = FakeEnv::new()
            .with_var("GH_TOKEN", "token")
            .with_var("SHDEPS_TEST_PLATFORM", "linux")
            .with_command("uname -m", "x86_64");
        let client = FakeClient::new().with(
            "https://api.github.com/repos/cgraf78/shdeps/releases",
            releases_json("v2026.05.24").into_bytes(),
        );

        let summary =
            release_archive(&dir, &metadata, &env, &FakeRunner::default(), &client).unwrap();

        assert_eq!(
            summary.outcome,
            ReleaseArchiveOutcome::NoUpdate {
                current: "v2026.05.24".to_owned(),
                candidate: "v2026.05.24".to_owned(),
            }
        );
        assert_eq!(summary.exit_code(), 0);
        assert_eq!(
            client.urls(),
            vec!["https://api.github.com/repos/cgraf78/shdeps/releases"]
        );
    }

    #[test]
    fn release_archive_self_update_stages_and_activates_release() {
        let root = temp_dir("release-update-root");
        let dir = root.join("shdeps");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("old"), "old").unwrap();
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.23".to_owned());
        metadata.repo = Some("cgraf78/shdeps".to_owned());
        let env = FakeEnv::new()
            .with_var("GH_TOKEN", "token")
            .with_var("SHDEPS_TEST_PLATFORM", "linux")
            .with_command("uname -m", "x86_64");
        let archive_name = "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz";
        let checksum_name = format!("{archive_name}.sha256");
        let archive = release_tar();
        let checksum = format!("{}  {archive_name}\n", checksum::sha256_hex(&archive));
        let client = FakeClient::new()
            .with(
                "https://api.github.com/repos/cgraf78/shdeps/releases",
                releases_json("v2026.05.24").into_bytes(),
            )
            .with(&format!("https://example/{archive_name}"), archive)
            .with(
                &format!("https://example/{checksum_name}"),
                checksum.into_bytes(),
            );

        let summary =
            release_archive(&dir, &metadata, &env, &FakeRunner::default(), &client).unwrap();

        assert_eq!(
            summary.outcome,
            ReleaseArchiveOutcome::Updated {
                tag: "v2026.05.24".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(dir.join("shdeps")).unwrap(),
            "new binary"
        );
        assert!(!dir.join("old").exists());
        let written = crate::install_metadata::read(&dir).unwrap();
        assert!(
            matches!(written, crate::install_metadata::Read::Valid(metadata)
                if metadata.tag.as_deref() == Some("v2026.05.24")
                    && metadata.artifact_platform.as_deref() == Some("linux-x86_64-musl"))
        );
        assert_eq!(
            client.tokens(),
            vec![
                Some("token".to_owned()),
                Some("token".to_owned()),
                Some("token".to_owned())
            ]
        );
    }

    #[test]
    fn release_archive_self_update_keeps_existing_install_on_bad_archive() {
        let root = temp_dir("release-bad-archive-root");
        let dir = root.join("shdeps");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("shdeps"), "old binary").unwrap();
        fs::write(dir.join("shdeps.sh"), "old shim").unwrap();
        let mut metadata = Metadata::new(Method::Release);
        metadata.tag = Some("v2026.05.23".to_owned());
        metadata.repo = Some("cgraf78/shdeps".to_owned());
        let env = FakeEnv::new()
            .with_var("GH_TOKEN", "token")
            .with_var("SHDEPS_TEST_PLATFORM", "linux")
            .with_command("uname -m", "x86_64");
        let archive_name = "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz";
        let checksum_name = format!("{archive_name}.sha256");
        let archive = release_tar();
        let client = FakeClient::new()
            .with(
                "https://api.github.com/repos/cgraf78/shdeps/releases",
                releases_json("v2026.05.24").into_bytes(),
            )
            .with(&format!("https://example/{archive_name}"), archive)
            .with(
                &format!("https://example/{checksum_name}"),
                b"bad checksum".to_vec(),
            );

        let failure =
            release_archive(&dir, &metadata, &env, &FakeRunner::default(), &client).unwrap_err();

        // A release archive is untrusted until checksum verification and
        // required-file validation both pass. A bad candidate must never move
        // the current install aside, because `self-update` runs from automated
        // bootstrap paths where the old binary is the only recovery tool.
        assert!(matches!(
            failure,
            ReleaseArchiveFailure::Stage(crate::release_stage::Failure::ChecksumMismatch { .. })
        ));
        assert_eq!(
            fs::read_to_string(dir.join("shdeps")).unwrap(),
            "old binary"
        );
        assert_eq!(
            fs::read_to_string(dir.join("shdeps.sh")).unwrap(),
            "old shim"
        );
        assert!(fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".shdeps-stage")));
    }

    fn checkout(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
        Release {
            tag: tag.to_owned(),
            draft,
            prerelease,
            assets: Vec::new(),
        }
    }

    fn release_with_assets(tag: &str, platform_labels: &[&str]) -> Release {
        let assets = platform_labels
            .iter()
            .flat_map(|label| {
                let archive = format!("shdeps-{tag}-{label}.tar.gz");
                let checksum = format!("{archive}.sha256");
                [
                    Asset {
                        name: archive.clone(),
                        url: format!("https://example/{archive}"),
                    },
                    Asset {
                        name: checksum.clone(),
                        url: format!("https://example/{checksum}"),
                    },
                ]
            })
            .collect();

        Release {
            tag: tag.to_owned(),
            draft: false,
            prerelease: false,
            assets,
        }
    }

    #[derive(Default)]
    struct FakeEnv {
        vars: BTreeMap<String, OsString>,
        commands: BTreeMap<String, String>,
    }

    impl FakeEnv {
        fn new() -> Self {
            Self::default()
        }

        fn with_var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_owned(), OsString::from(value));
            self
        }

        fn with_command(mut self, command: &str, output: &str) -> Self {
            self.commands.insert(command.to_owned(), output.to_owned());
            self
        }
    }

    impl Env for FakeEnv {
        fn var_os(&self, name: &str) -> Option<OsString> {
            self.vars.get(name).cloned()
        }

        fn command_output(&self, command: &str, args: &[&str]) -> Option<String> {
            self.commands
                .get(&format!("{command} {}", args.join(" ")))
                .cloned()
        }

        fn read_to_string(&self, _path: &Path) -> Option<String> {
            None
        }
    }

    #[derive(Default)]
    struct FakeClient {
        responses: BTreeMap<String, Vec<u8>>,
        calls: std::cell::RefCell<Vec<(String, Option<String>)>>,
    }

    impl FakeClient {
        fn new() -> Self {
            Self::default()
        }

        fn with(mut self, url: &str, body: Vec<u8>) -> Self {
            self.responses.insert(url.to_owned(), body);
            self
        }

        fn urls(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(url, _)| url.clone())
                .collect()
        }

        fn tokens(&self) -> Vec<Option<String>> {
            self.calls
                .borrow()
                .iter()
                .map(|(_, token)| token.clone())
                .collect()
        }
    }

    impl Client for FakeClient {
        fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls
                .borrow_mut()
                .push((url.to_owned(), token.map(ToOwned::to_owned)));
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url.to_owned()))
        }
    }

    fn releases_json(tag: &str) -> String {
        let archive_name = format!("shdeps-{tag}-linux-x86_64-musl.tar.gz");
        format!(
            r#"[{{
              "tag_name": "{tag}",
              "draft": false,
              "prerelease": false,
              "assets": [
                {{"name":"{archive_name}","browser_download_url":"https://example/{archive_name}"}},
                {{"name":"{archive_name}.sha256","browser_download_url":"https://example/{archive_name}.sha256"}}
              ]
            }}]"#
        )
    }

    fn release_tar() -> Vec<u8> {
        let files = [
            ("shdeps", &b"new binary"[..]),
            ("shdeps.sh", &b"shim"[..]),
            ("install.sh", &b"install"[..]),
            ("README.md", &b"readme"[..]),
            ("LICENSE", &b"license"[..]),
            ("man/man1/shdeps.1", &b"man"[..]),
        ];
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for (path, body) in files {
                let mut header = Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append(&header, body).unwrap();
            }
            builder.finish().unwrap();
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
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
