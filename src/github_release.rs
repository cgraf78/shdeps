//! GitHub release dependency selection.
//!
//! The generic `github:release` method has a different contract from
//! shdeps' own release archives: third-party projects publish many asset
//! naming conventions, and shdeps must choose the asset that looks like the
//! requested command for the current host. This module keeps that policy
//! separate from download/install side effects so update code can be tested
//! without touching the network.

use crate::github::Release;
use crate::platform::RuntimeEnv;
use crate::process::Runner;
use crate::release_asset::{self, Target};

/// Concrete release asset selected for a `github:release` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Git tag selected from stable, non-draft releases.
    pub tag: String,
    /// Download URL for the host-compatible install asset.
    pub url: String,
    /// REST API URL for authenticated private-release fallback.
    pub api_url: Option<String>,
    /// Bare asset file name as published in the release. Carried alongside
    /// `url` so the checksum-verification step can bind the digest to the
    /// asset name (the same filename appears as the second field in standard
    /// `sha256sum` output) instead of relying on positional trust.
    pub asset_name: String,
    /// Browser URL of the sibling SHA-256 checksum asset if the upstream
    /// release published one (looked up as `<asset_name>.sha256`). `None`
    /// when the release does not include a per-asset digest, in which case
    /// installation proceeds unverified for backward compatibility — see
    /// the comment in `update_release::install_request`.
    pub checksum_url: Option<String>,
    /// REST API URL for the sibling checksum asset (private-release fallback).
    pub checksum_api_url: Option<String>,
}

/// Returns the release that GitHub would expose as "latest" for third-party deps.
///
/// The releases API is already ordered by GitHub's publication recency. Keeping
/// this helper public inside the crate lets update logic compare the installed
/// binary version before doing asset matching or downloads, which preserves the
/// Bash contract that `--force` means "check now", not "reinstall now".
#[must_use]
pub(crate) fn latest_stable(releases: &[Release]) -> Option<&Release> {
    releases
        .iter()
        .find(|release| !release.draft && !release.prerelease)
}

/// Selects the best install asset for a command on the current host.
#[must_use]
pub fn select(
    cmd: &str,
    releases: &[Release],
    env: &RuntimeEnv,
    runner: &impl Runner,
) -> Option<Selection> {
    // Bash asks GitHub for `/releases/latest`, which is GitHub's latest
    // non-draft/non-prerelease release by publication order. The full releases
    // API returns newest first, so choose the first stable release instead of
    // sorting tags. Third-party projects often use tags that are not monotonic
    // under shdeps' own release-version comparison.
    let release = latest_stable(releases)?;
    let urls = release
        .assets
        .iter()
        .map(|asset| asset.url.as_str())
        .collect::<Vec<_>>();
    let target = target(env, runner);
    let url = release_asset::select(cmd, &urls, &target)?.to_owned();
    let primary = release.assets.iter().find(|asset| asset.url == url)?;
    let api_url = primary.api_url.clone();
    // Standard practice for third-party GitHub releases is to publish a
    // sibling checksum asset named `<asset>.sha256` (sometimes `.sha512`,
    // but `.sha256` is overwhelmingly the common form). When the upstream
    // publishes one, install code uses it to verify the binary before
    // landing it on disk. We do not invent or fall back to other digests:
    // a mismatched name silently downgrading the trust model is exactly
    // the failure shape we want to avoid.
    let checksum_name = format!("{}.sha256", primary.name);
    let checksum_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name);

    Some(Selection {
        tag: release.tag.clone(),
        url,
        api_url,
        asset_name: primary.name.clone(),
        checksum_url: checksum_asset.map(|asset| asset.url.clone()),
        checksum_api_url: checksum_asset.and_then(|asset| asset.api_url.clone()),
    })
}

fn target(env: &RuntimeEnv, runner: &impl Runner) -> Target {
    let os = match env.platform() {
        // Bash uses `uname -s` directly, so macOS appears as `darwin` in the
        // release matcher. Preserve that spelling because many upstream
        // projects use `darwin` instead of `macos` in asset names.
        "macos" => "darwin".to_owned(),
        // WSL consumes normal Linux third-party release assets; the separate
        // musl-only naming rule applies only to shdeps' own release archives.
        "wsl" => "linux".to_owned(),
        other => other.to_owned(),
    };
    Target::new(os, arch(runner), libc(runner))
}

fn arch(runner: &impl Runner) -> String {
    runner
        .run("uname", &["-m"], None)
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

fn libc(runner: &impl Runner) -> &'static str {
    if !runner.exists("ldd") {
        return "gnu";
    }
    let Ok(output) = runner.run("ldd", &["--version"], None) else {
        return "gnu";
    };
    let combined = format!("{}{}", output.stdout, output.stderr);
    if combined.to_ascii_lowercase().contains("musl") {
        "musl"
    } else {
        "gnu"
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use super::select;
    use crate::github::{Asset, Release};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};

    #[test]
    fn select_skips_drafts_prereleases_and_matches_current_host() {
        let releases = vec![
            release("v2.0.0", true, false, &["tool-v2.0.0-linux-x86_64.tar.gz"]),
            release("v1.9.0", false, true, &["tool-v1.9.0-linux-x86_64.tar.gz"]),
            release("v1.8.0", false, false, &["tool-v1.8.0-linux-x86_64.tar.gz"]),
        ];
        let runner = FakeRunner::new("x86_64", "");

        assert_eq!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("linux", "host"),
                &runner
            ),
            Some(super::Selection {
                tag: "v1.8.0".to_owned(),
                url: "https://github.com/owner/tool/releases/download/v1/tool-v1.8.0-linux-x86_64.tar.gz".to_owned(),
                api_url: None,
                asset_name: "tool-v1.8.0-linux-x86_64.tar.gz".to_owned(),
                checksum_url: None,
                checksum_api_url: None,
            })
        );
    }

    #[test]
    fn select_finds_sibling_sha256_checksum_asset_when_published() {
        // A well-published third-party release ships `<asset>.sha256`
        // alongside each binary asset. The selector must surface that
        // sibling so the install path can verify the download before
        // landing it on disk.
        let releases = vec![Release {
            tag: "v1.0.0".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: "tool-v1.0.0-linux-x86_64.tar.gz".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz".to_owned(),
                    api_url: None,
                },
                Asset {
                    name: "tool-v1.0.0-linux-x86_64.tar.gz.sha256".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.sha256".to_owned(),
                    api_url: None,
                },
            ],
        }];
        let runner = FakeRunner::new("x86_64", "");

        let selection = select(
            "tool",
            &releases,
            &RuntimeEnv::new("linux", "host"),
            &runner,
        )
        .unwrap();

        assert_eq!(
            selection.checksum_url.as_deref(),
            Some(
                "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.sha256"
            )
        );
        assert_eq!(selection.asset_name, "tool-v1.0.0-linux-x86_64.tar.gz");
    }

    #[test]
    fn select_returns_no_checksum_when_release_has_only_binary_asset() {
        // Many older releases ship only the binary. Verification then runs
        // in best-effort mode (no checksum → proceed unverified), so the
        // selector must explicitly model that as `None` rather than
        // invent a digest URL the install layer would 404 on.
        let releases = vec![release(
            "v1.0.0",
            false,
            false,
            &["tool-v1.0.0-linux-x86_64.tar.gz"],
        )];
        let runner = FakeRunner::new("x86_64", "");

        let selection = select(
            "tool",
            &releases,
            &RuntimeEnv::new("linux", "host"),
            &runner,
        )
        .unwrap();

        assert_eq!(selection.checksum_url, None);
        assert_eq!(selection.checksum_api_url, None);
    }

    #[test]
    fn select_uses_darwin_alias_for_macos_runtime() {
        let releases = vec![release(
            "v1.0.0",
            false,
            false,
            &["tool-v1.0.0-darwin-arm64.zip"],
        )];
        let runner = FakeRunner::new("arm64", "");

        assert!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("macos", "host"),
                &runner
            )
            .is_some()
        );
    }

    #[test]
    fn select_prefers_matching_linux_libc_when_available() {
        let releases = vec![release(
            "v1.0.0",
            false,
            false,
            &[
                "tool-v1.0.0-linux-x86_64-gnu.tar.gz",
                "tool-v1.0.0-linux-x86_64-musl.tar.gz",
            ],
        )];
        let runner = FakeRunner::new("x86_64", "musl libc");

        assert_eq!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("linux", "host"),
                &runner
            )
            .unwrap()
            .url,
            "https://github.com/owner/tool/releases/download/v1/tool-v1.0.0-linux-x86_64-musl.tar.gz"
        );
    }

    #[test]
    fn select_keeps_api_asset_url_for_private_download_fallback() {
        let releases = vec![Release {
            tag: "v1.0.0".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "tool-linux-x86_64".to_owned(),
                url: "https://private.example/tool-linux-x86_64".to_owned(),
                api_url: Some(
                    "https://api.github.com/repos/owner/tool/releases/assets/7".to_owned(),
                ),
            }],
        }];
        let runner = FakeRunner::new("x86_64", "");

        assert_eq!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("linux", "host"),
                &runner
            )
            .unwrap()
            .api_url
            .as_deref(),
            Some("https://api.github.com/repos/owner/tool/releases/assets/7")
        );
    }

    #[test]
    fn select_preserves_github_latest_order_instead_of_sorting_tags() {
        let releases = vec![
            release("v1.0.0", false, false, &["tool-v1.0.0-linux-x86_64.tar.gz"]),
            release("v9.0.0", false, false, &["tool-v9.0.0-linux-x86_64.tar.gz"]),
        ];
        let runner = FakeRunner::new("x86_64", "");

        assert_eq!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("linux", "host"),
                &runner
            )
            .unwrap()
            .tag,
            "v1.0.0"
        );
    }

    fn release(tag: &str, draft: bool, prerelease: bool, names: &[&str]) -> Release {
        Release {
            tag: tag.to_owned(),
            draft,
            prerelease,
            assets: names
                .iter()
                .map(|name| Asset {
                    name: (*name).to_owned(),
                    url: format!("https://github.com/owner/tool/releases/download/v1/{name}"),
                    api_url: None,
                })
                .collect(),
        }
    }

    #[derive(Debug, Clone)]
    struct FakeRunner {
        arch: String,
        ldd: String,
    }

    impl FakeRunner {
        fn new(arch: &str, ldd: &str) -> Self {
            Self {
                arch: arch.to_owned(),
                ldd: ldd.to_owned(),
            }
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, command: &str) -> bool {
            command == "ldd" && !self.ldd.is_empty()
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            match (program, args) {
                ("uname", ["-m"]) => Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: self.arch.clone(),
                    stderr: String::new(),
                }),
                ("ldd", ["--version"]) => Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: self.ldd.clone(),
                    stderr: String::new(),
                }),
                _ => Ok(Output {
                    success: false,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
    }
}
