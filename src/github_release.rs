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
    /// Browser URL of a checksum asset if the upstream release published one.
    /// `None` when the release does not include a recognized per-asset or
    /// release-wide checksum file, in which case installation proceeds
    /// unverified for backward compatibility — see the comment in
    /// `update_release::install_request`.
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
    // Prefer asset-specific checksums, then release-wide checksum files whose
    // contents still bind each digest to an exact filename. The install layer
    // verifies that named binding before landing bytes on disk.
    let checksum_asset = checksum_asset(&release.assets, &primary.name);

    Some(Selection {
        tag: release.tag.clone(),
        url,
        api_url,
        asset_name: primary.name.clone(),
        checksum_url: checksum_asset.map(|asset| asset.url.clone()),
        checksum_api_url: checksum_asset.and_then(|asset| asset.api_url.clone()),
    })
}

fn checksum_asset<'a>(
    assets: &'a [crate::github::Asset],
    primary_name: &str,
) -> Option<&'a crate::github::Asset> {
    let primary_name = primary_name.to_ascii_lowercase();
    let sibling_names = [
        format!("{primary_name}.sha256"),
        format!("{primary_name}.sha256sum"),
        format!("{primary_name}.sha512"),
        format!("{primary_name}.sha512sum"),
    ];
    if let Some(asset) = assets.iter().find(|asset| {
        let name = asset.name.to_ascii_lowercase();
        sibling_names.contains(&name)
    }) {
        return Some(asset);
    }

    let mut best = None;
    for asset in assets {
        let Some(priority) = release_wide_checksum_priority(&asset.name) else {
            continue;
        };
        if best.is_none_or(|(_, best_priority)| priority < best_priority) {
            best = Some((asset, priority));
        }
    }
    best.map(|(asset, _)| asset)
}

fn release_wide_checksum_priority(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "sha256.sum"
            | "sha256sum"
            | "sha256sum.txt"
            | "sha256sums"
            | "sha256sums.txt"
            | "sha512.sum"
            | "sha512sum"
            | "sha512sum.txt"
            | "sha512sums"
            | "sha512sums.txt"
            | "shasums256"
            | "shasums256.txt"
            | "shasums512"
            | "shasums512.txt"
    ) {
        return Some(0);
    }
    if matches!(
        name.as_str(),
        "checksums" | "checksums.txt" | "checksum" | "checksum.txt"
    ) || name.ends_with("_checksums.txt")
        || name.ends_with("-checksums.txt")
    {
        return Some(1);
    }
    None
}

fn target(env: &RuntimeEnv, runner: &impl Runner) -> Target {
    let os = match env.platform() {
        "linux" if env.is_android() => "android".to_owned(),
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
        .run(
            "uname",
            &["-m"],
            Some(crate::process::VERSION_PROBE_TIMEOUT),
        )
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

fn libc(runner: &impl Runner) -> &'static str {
    libc_with_probe(runner, host_musl_ld_present())
}

/// Pure decision function for libc identity, separated from FS probing so
/// tests can inject the musl-loader-present signal without touching the
/// real `/lib` tree.
fn libc_with_probe(runner: &impl Runner, musl_ld_present: bool) -> &'static str {
    // Preferred signal: `ldd --version` text. On glibc systems this prints
    // a string mentioning glibc; on musl it prints "musl libc". When ldd
    // is present and the output is parseable, trust it — it is the most
    // precise indicator the host can offer.
    if runner.exists("ldd") {
        if let Ok(output) = runner.run(
            "ldd",
            &["--version"],
            Some(crate::process::VERSION_PROBE_TIMEOUT),
        ) {
            let combined = format!("{}{}", output.stdout, output.stderr);
            if combined.to_ascii_lowercase().contains("musl") {
                return "musl";
            }
            // Successful ldd output without "musl" → glibc. This is the
            // common case on Debian/Ubuntu/RHEL/Fedora.
            return "gnu";
        }
    }

    // Fallback: no usable ldd output. Defaulting to "gnu" used to silently
    // select gnu binaries on Alpine-minimal, NixOS minimal profiles,
    // and static-link-only container images that ship a musl loader but
    // no `ldd` wrapper. Look for the canonical musl loader on disk
    // before falling through to the gnu default.
    if musl_ld_present {
        return "musl";
    }
    "gnu"
}

/// Returns whether the host has a musl dynamic loader at the canonical
/// path (`/lib/ld-musl-*.so.1`). Used as the fallback signal when `ldd`
/// is absent or malformed.
fn host_musl_ld_present() -> bool {
    // Iterating `/lib` is cheap (one stat per entry) and works for any
    // arch suffix musl publishes (x86_64, aarch64, armhf, ...). Reading
    // the dir rather than checking a fixed list keeps this correct on
    // arches we have not enumerated here.
    let Ok(entries) = std::fs::read_dir("/lib") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("ld-musl-") && name.ends_with(".so.1"))
    })
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
    fn libc_with_probe_prefers_ldd_output_over_filesystem_signal() {
        // When `ldd --version` says musl, that is the precise signal —
        // do not be swayed by a stray musl-loader file on a glibc-primary
        // host (theoretical, but the test pins the precedence).
        let runner = FakeRunner::new("x86_64", "musl libc x86_64");
        assert_eq!(super::libc_with_probe(&runner, false), "musl");

        // ldd present with glibc-ish output → gnu, regardless of any
        // musl-loader presence (no real host has both as primary, but
        // again the precedence is what is being pinned).
        let runner = FakeRunner::new("x86_64", "GNU libc 2.39");
        assert_eq!(super::libc_with_probe(&runner, true), "gnu");
    }

    #[test]
    fn libc_with_probe_falls_back_to_musl_loader_when_ldd_absent() {
        // Alpine-minimal images and NixOS minimal profiles can ship a
        // musl loader at `/lib/ld-musl-*.so.1` but no `ldd` wrapper at
        // all. The pre-fix code defaulted to "gnu" in that case, which
        // installed glibc-linked binaries that crashed at runtime. The
        // fallback to the filesystem probe fixes that without disturbing
        // the normal `ldd`-present paths.
        let runner = NoLddRunner;
        assert_eq!(super::libc_with_probe(&runner, true), "musl");
    }

    #[test]
    fn libc_with_probe_defaults_to_gnu_when_no_signals_available() {
        // No ldd, no musl loader → gnu. This preserves the legacy
        // default for hosts that legitimately have neither (e.g.,
        // statically-linked busybox-only systems where the choice does
        // not matter), and keeps the behavior the same as before this
        // change for the majority of dev machines.
        let runner = NoLddRunner;
        assert_eq!(super::libc_with_probe(&runner, false), "gnu");
    }

    /// Runner where `ldd` is reported absent, used to exercise the
    /// FS-probe fallback path in `libc_with_probe`.
    struct NoLddRunner;
    impl Runner for NoLddRunner {
        fn exists(&self, _command: &str) -> bool {
            false
        }
        fn run(
            &self,
            _program: &str,
            _args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
        }
    }

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
    fn select_finds_sibling_checksum_asset_case_insensitively() {
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
                    name: "tool-v1.0.0-linux-x86_64.tar.gz.SHA512SUM".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.SHA512SUM".to_owned(),
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
                "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.SHA512SUM"
            )
        );
    }

    #[test]
    fn select_finds_common_checksum_asset_variants() {
        let runner = FakeRunner::new("x86_64", "");

        let sibling_sha512 = vec![Release {
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
                    name: "tool-v1.0.0-linux-x86_64.tar.gz.sha512sum".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.sha512sum".to_owned(),
                    api_url: None,
                },
            ],
        }];
        assert_eq!(
            select(
                "tool",
                &sibling_sha512,
                &RuntimeEnv::new("linux", "host"),
                &runner
            )
            .unwrap()
            .checksum_url
            .as_deref(),
            Some(
                "https://github.com/owner/tool/releases/download/v1.0.0/tool-v1.0.0-linux-x86_64.tar.gz.sha512sum"
            )
        );

        let release_wide = vec![Release {
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
                    name: "SHA256SUMS".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/SHA256SUMS".to_owned(),
                    api_url: None,
                },
            ],
        }];
        assert_eq!(
            select(
                "tool",
                &release_wide,
                &RuntimeEnv::new("linux", "host"),
                &runner
            )
            .unwrap()
            .checksum_url
            .as_deref(),
            Some("https://github.com/owner/tool/releases/download/v1.0.0/SHA256SUMS")
        );

        for checksum_name in [
            "sha256.sum",
            "sha256sum.txt",
            "SHASUMS256.txt",
            "tool_1.2.3_checksums.txt",
        ] {
            let release_wide = vec![Release {
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
                        name: checksum_name.to_owned(),
                        url: format!(
                            "https://github.com/owner/tool/releases/download/v1.0.0/{checksum_name}"
                        ),
                        api_url: None,
                    },
                ],
            }];

            assert_eq!(
                select(
                    "tool",
                    &release_wide,
                    &RuntimeEnv::new("linux", "host"),
                    &runner
                )
                .unwrap()
                .checksum_url
                .as_deref(),
                Some(format!(
                    "https://github.com/owner/tool/releases/download/v1.0.0/{checksum_name}"
                ))
                .as_deref(),
                "{checksum_name}"
            );
        }
    }

    #[test]
    fn select_prefers_specific_release_wide_checksum_over_generic_manifest() {
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
                    name: "checksums.txt".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/checksums.txt".to_owned(),
                    api_url: None,
                },
                Asset {
                    name: "SHA256SUMS".to_owned(),
                    url: "https://github.com/owner/tool/releases/download/v1.0.0/SHA256SUMS".to_owned(),
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
            Some("https://github.com/owner/tool/releases/download/v1.0.0/SHA256SUMS")
        );
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
    fn select_uses_android_assets_inside_termux() {
        let releases = vec![release(
            "v1.0.0",
            false,
            false,
            &[
                "tool-v1.0.0-linux-aarch64-musl.tar.gz",
                "tool-v1.0.0-android-aarch64.tar.gz",
            ],
        )];
        let runner = FakeRunner::new("aarch64", "");

        assert_eq!(
            select(
                "tool",
                &releases,
                &RuntimeEnv::new("linux", "host").with_android(true),
                &runner
            )
            .unwrap()
            .url,
            "https://github.com/owner/tool/releases/download/v1/tool-v1.0.0-android-aarch64.tar.gz"
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
