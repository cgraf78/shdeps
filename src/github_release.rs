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
    let release = releases
        .iter()
        .find(|release| !release.draft && !release.prerelease)?;
    let urls = release
        .assets
        .iter()
        .map(|asset| asset.url.as_str())
        .collect::<Vec<_>>();
    let target = target(env, runner);
    let url = release_asset::select(cmd, &urls, &target)?.to_owned();

    Some(Selection {
        tag: release.tag.clone(),
        url,
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
                url: "https://example/tool-v1.8.0-linux-x86_64.tar.gz".to_owned(),
            })
        );
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

        assert!(select(
            "tool",
            &releases,
            &RuntimeEnv::new("macos", "host"),
            &runner
        )
        .is_some());
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
            "https://example/tool-v1.0.0-linux-x86_64-musl.tar.gz"
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
                    url: format!("https://example/{name}"),
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
