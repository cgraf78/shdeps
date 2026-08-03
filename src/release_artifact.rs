//! shdeps release artifact naming and host-platform selection.
//!
//! `release_asset` handles generic third-party asset matching, where projects
//! publish many naming schemes. The shdeps release workflow is intentionally
//! stricter: every self-update archive has one exact platform label and one
//! adjacent checksum file. Keeping that contract in its own module gives the
//! installer and `self-update` one shared source of truth and prevents fallback
//! guesses that could install a nearby-but-wrong binary on a fleet machine.

use std::path::Path;

use crate::github::{Asset, Release};
use crate::runtime::{self, Env};

/// Installer-facing platform labels published by the shdeps release workflow.
pub(crate) const PLATFORM_LABELS: &[&str] = &[
    "linux-x86_64-musl",
    "linux-aarch64-musl",
    "android-aarch64",
    "android-x86_64",
    "macos-x86_64",
    "macos-aarch64",
];

/// Archive/checksum pair selected for a shdeps release update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    /// Expected archive file name from the release contract.
    pub archive_name: String,
    /// Archive download URL.
    pub archive_url: String,
    /// REST API URL for authenticated archive fallback.
    pub archive_api_url: Option<String>,
    /// Expected checksum file name from the release contract.
    pub checksum_name: String,
    /// Checksum download URL.
    pub checksum_url: String,
    /// REST API URL for authenticated checksum fallback.
    pub checksum_api_url: Option<String>,
}

/// Returns the supported shdeps artifact label for the current host.
#[must_use]
pub fn host_label(env: &impl Env) -> Option<String> {
    let runtime = runtime::runtime_env(env);
    let arch_probe = env
        .command_output("uname", &["-m"])
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
    let arch = normalize_arch(&arch_probe)?;

    match runtime.platform() {
        "linux" if runtime.is_android() => Some(format!("android-{arch}")),
        // The release workflow publishes musl Linux binaries so older fleet
        // machines and WSL installs do not inherit the builder's glibc floor.
        "linux" | "wsl" => Some(format!("linux-{arch}-musl")),
        "macos" => Some(format!("macos-{arch}")),
        _ => None,
    }
}

/// Selects the exact archive/checksum pair for `release` and `platform_label`.
#[must_use]
pub fn select_pair(release: &Release, platform_label: &str) -> Option<Pair> {
    let archive_name = format!("shdeps-{}-{platform_label}.tar.gz", release.tag);
    let checksum_name = format!("{archive_name}.sha256");
    let archive = find_asset(&release.assets, &archive_name)?;
    let checksum = find_asset(&release.assets, &checksum_name)?;

    Some(Pair {
        archive_name,
        archive_url: archive.url.clone(),
        archive_api_url: archive.api_url.clone(),
        checksum_name,
        checksum_url: checksum.url.clone(),
        checksum_api_url: checksum.api_url.clone(),
    })
}

fn normalize_arch(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

fn find_asset<'a>(assets: &'a [Asset], expected_name: &str) -> Option<&'a Asset> {
    assets.iter().find(|asset| {
        if asset.name == expected_name {
            return true;
        }

        // GitHub's `name` field is the authoritative contract, but keeping a
        // URL-basename fallback makes fixtures and mirrors less brittle without
        // relaxing the platform label. It still refuses nearby labels because
        // the expected full file name must match exactly.
        Path::new(asset.url.rsplit('/').next().unwrap_or(&asset.url))
            .file_name()
            .and_then(|name| name.to_str())
            == Some(expected_name)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;

    use super::{Pair, host_label, select_pair};
    use crate::github::{Asset, Release};
    use crate::runtime::Env;

    #[test]
    fn host_label_maps_supported_release_platforms() {
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "linux")
                    .with_var("TERMUX_VERSION", "0.118.3")
                    .with_command("uname -m", "aarch64")
            ),
            Some("android-aarch64".to_owned())
        );
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "linux")
                    .with_var("TERMUX_VERSION", "0.118.3")
                    .with_command("uname -m", "x86_64")
            ),
            Some("android-x86_64".to_owned())
        );
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "linux")
                    .with_command("uname -m", "amd64")
            ),
            Some("linux-x86_64-musl".to_owned())
        );
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "wsl")
                    .with_command("uname -m", "aarch64")
            ),
            Some("linux-aarch64-musl".to_owned())
        );
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "macos")
                    .with_command("uname -m", "arm64")
            ),
            Some("macos-aarch64".to_owned())
        );
    }

    #[test]
    fn host_label_rejects_unknown_platforms_and_architectures() {
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "freebsd")
                    .with_command("uname -m", "amd64")
            ),
            None
        );
        assert_eq!(
            host_label(
                &FakeEnv::new()
                    .with_var("SHDEPS_TEST_PLATFORM", "linux")
                    .with_command("uname -m", "riscv64")
            ),
            None
        );
    }

    #[test]
    fn select_pair_requires_exact_archive_and_checksum() {
        let release = release(&[
            (
                "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz",
                "https://github.com/owner/tool/releases/download/v1/archive",
            ),
            (
                "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz.sha256",
                "https://github.com/owner/tool/releases/download/v1/checksum",
            ),
            (
                "shdeps-v2026.05.24-linux-aarch64-musl.tar.gz",
                "https://github.com/owner/tool/releases/download/v1/wrong-arch",
            ),
        ]);

        assert_eq!(
            select_pair(&release, "linux-x86_64-musl"),
            Some(Pair {
                archive_name: "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz".to_owned(),
                archive_url: "https://github.com/owner/tool/releases/download/v1/archive"
                    .to_owned(),
                archive_api_url: None,
                checksum_name: "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz.sha256".to_owned(),
                checksum_url: "https://github.com/owner/tool/releases/download/v1/checksum"
                    .to_owned(),
                checksum_api_url: None,
            })
        );
        assert_eq!(select_pair(&release, "macos-aarch64"), None);
    }

    #[test]
    fn select_pair_can_use_url_basename_when_asset_name_is_empty() {
        let release = Release {
            tag: "v2026.05.24".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: String::new(),
                    url: "https://github.com/owner/tool/releases/download/v1/shdeps-v2026.05.24-macos-aarch64.tar.gz".to_owned(),
                    api_url: None,
                },
                Asset {
                    name: String::new(),
                    url: "https://github.com/owner/tool/releases/download/v1/shdeps-v2026.05.24-macos-aarch64.tar.gz.sha256"
                        .to_owned(),
                    api_url: None,
                },
            ],
        };

        assert!(select_pair(&release, "macos-aarch64").is_some());
    }

    #[test]
    fn select_pair_rejects_missing_checksum() {
        let release = release(&[(
            "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz",
            "https://github.com/owner/tool/releases/download/v1/archive",
        )]);

        assert_eq!(select_pair(&release, "linux-x86_64-musl"), None);
    }

    fn release(assets: &[(&str, &str)]) -> Release {
        Release {
            tag: "v2026.05.24".to_owned(),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|(name, url)| Asset {
                    name: (*name).to_owned(),
                    url: (*url).to_owned(),
                    api_url: None,
                })
                .collect(),
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
}
