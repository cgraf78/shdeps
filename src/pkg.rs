//! Package-manager command planning.
//!
//! `pkg` dependencies are the only install method that can cross into
//! system-owned state. Keep the manager-specific command shapes here so update,
//! custom hook helpers, and future dry-run diagnostics cannot drift into subtly
//! different sudo/install behavior.

use crate::platform::RuntimeEnv;

/// Privilege boundary used for package-manager mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Execute the package manager as the current user.
    Direct,
    /// Execute the package manager through `sudo`.
    Sudo,
}

impl Elevation {
    /// Resolves the package privilege model for a runtime.
    ///
    /// Termux owns its package prefix as the Android app user and deliberately
    /// does not use system-style root escalation. Other Unix package managers
    /// retain the existing sudo contract.
    #[must_use]
    pub fn for_manager(mgr: &str, env: &RuntimeEnv) -> Self {
        if env.is_android() && mgr == "apt" {
            Self::Direct
        } else {
            Self::Sudo
        }
    }
}

/// Shell command and argument vector to run for one package-manager action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name.
    pub program: String,
    /// Arguments passed without shell interpolation.
    pub args: Vec<String>,
}

impl CommandSpec {
    fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Builds the package availability probe used before queueing an install.
///
/// Unavailable package-manager deps are compatibility-skips, not hard failures,
/// so availability probing is a first-class decision instead of an incidental
/// failed install. The commands mirror Bash's `_shdeps_pkg_available` branch.
#[must_use]
pub fn available(mgr: &str, package: &str) -> Option<CommandSpec> {
    match mgr {
        "brew" => Some(CommandSpec::new("brew", ["info", package])),
        "apt" => Some(CommandSpec::new("apt-cache", ["show", package])),
        "dnf" => Some(CommandSpec::new("dnf", ["info", package])),
        "pacman" => Some(CommandSpec::new("pacman", ["-Si", package])),
        "zypper" => Some(CommandSpec::new("zypper", ["info", package])),
        "apk" => Some(CommandSpec::new("apk", ["search", "-e", package])),
        _ => None,
    }
}

/// Interprets package availability probe output.
///
/// Most managers communicate availability through the exit status. `apk` is
/// different in the Bash reference because `apk search -e` is piped through
/// `grep -q .`; an empty successful search is still treated as unavailable.
#[must_use]
pub fn available_ok(mgr: &str, success: bool, stdout: &str) -> bool {
    if mgr == "apk" {
        success && !stdout.trim().is_empty()
    } else {
        success
    }
}

/// Builds the metadata refresh command for managers with mutable repo caches.
#[must_use]
pub fn refresh(mgr: &str, elevation: Elevation) -> Option<CommandSpec> {
    let command = match mgr {
        // Homebrew does its own metadata management around install/info. Bash
        // also skips an explicit brew refresh, which keeps normal macOS update
        // runs from paying a surprising `brew update` cost.
        "brew" => return None,
        "apt" => CommandSpec::new("apt-get", ["update", "-qq"]),
        "dnf" => CommandSpec::new("dnf", ["makecache", "-q"]),
        "pacman" => CommandSpec::new("pacman", ["-Sy"]),
        "zypper" => CommandSpec::new("zypper", ["-q", "refresh"]),
        "apk" => CommandSpec::new("apk", ["update"]),
        _ => return None,
    };
    Some(elevate(command, elevation))
}

/// Builds the batched install command for one or more packages.
///
/// The Rust updater should preserve batching because package-manager startup
/// dominates cold installs. Callers should pass only packages that survived
/// availability checks; an empty package list intentionally returns `None`.
#[must_use]
pub fn install(mgr: &str, packages: &[String], elevation: Elevation) -> Option<CommandSpec> {
    if packages.is_empty() {
        return None;
    }

    let (program, mut args) = match mgr {
        "brew" => ("brew", vec!["install".to_owned()]),
        "apt" => ("apt-get", vec!["install".to_owned(), "-y".to_owned()]),
        "dnf" => ("dnf", vec!["install".to_owned(), "-y".to_owned()]),
        "pacman" => (
            "pacman",
            vec![
                "-Sy".to_owned(),
                "--needed".to_owned(),
                "--noconfirm".to_owned(),
            ],
        ),
        "zypper" => ("zypper", vec!["-n".to_owned(), "install".to_owned()]),
        "apk" => ("apk", vec!["add".to_owned()]),
        _ => return None,
    };
    args.extend(packages.iter().cloned());

    if mgr == "brew" {
        Some(CommandSpec::new(program, args))
    } else {
        Some(elevate(CommandSpec::new(program, args), elevation))
    }
}

fn elevate(command: CommandSpec, elevation: Elevation) -> CommandSpec {
    if elevation == Elevation::Direct {
        return command;
    }
    let mut args = Vec::with_capacity(command.args.len() + 1);
    args.push(command.program);
    args.extend(command.args);
    CommandSpec::new("sudo", args)
}

#[cfg(test)]
mod tests {
    use super::{CommandSpec, Elevation, available, install, refresh};
    use crate::platform::RuntimeEnv;

    #[test]
    fn android_package_mutations_run_directly() {
        let android = RuntimeEnv::new("linux", "phone").with_android(true);
        let elevation = Elevation::for_manager("apt", &android);
        let packages = vec!["jq".to_owned(), "fd".to_owned()];

        assert_eq!(elevation, Elevation::Direct);
        assert_eq!(
            refresh("apt", elevation),
            Some(cmd("apt-get", ["update", "-qq"]))
        );
        assert_eq!(
            install("apt", &packages, elevation),
            Some(cmd("apt-get", ["install", "-y", "jq", "fd"]))
        );
    }

    #[test]
    fn ordinary_linux_package_mutations_keep_sudo() {
        let linux = RuntimeEnv::new("linux", "server");
        assert_eq!(Elevation::for_manager("apt", &linux), Elevation::Sudo);
    }

    #[test]
    fn android_non_termux_managers_keep_sudo() {
        let android = RuntimeEnv::new("linux", "phone").with_android(true);
        assert_eq!(Elevation::for_manager("dnf", &android), Elevation::Sudo);
    }

    #[test]
    fn availability_probes_match_bash_manager_branches() {
        assert_eq!(available("brew", "jq"), Some(cmd("brew", ["info", "jq"])));
        assert_eq!(
            available("apt", "jq"),
            Some(cmd("apt-cache", ["show", "jq"]))
        );
        assert_eq!(available("dnf", "jq"), Some(cmd("dnf", ["info", "jq"])));
        assert_eq!(
            available("pacman", "jq"),
            Some(cmd("pacman", ["-Si", "jq"]))
        );
        assert_eq!(
            available("zypper", "jq"),
            Some(cmd("zypper", ["info", "jq"]))
        );
        assert_eq!(
            available("apk", "jq"),
            Some(cmd("apk", ["search", "-e", "jq"]))
        );
        assert_eq!(available("unknown", "jq"), None);
    }

    #[test]
    fn availability_success_preserves_apk_non_empty_output_requirement() {
        assert!(super::available_ok("apt", true, ""));
        assert!(!super::available_ok("apt", false, "Package: jq\n"));
        assert!(super::available_ok("apk", true, "jq-1.7-r0\n"));
        assert!(!super::available_ok("apk", true, ""));
    }

    #[test]
    fn refresh_skips_brew_and_uses_quiet_metadata_commands() {
        assert_eq!(refresh("brew", Elevation::Sudo), None);
        assert_eq!(
            refresh("apt", Elevation::Sudo),
            Some(cmd("sudo", ["apt-get", "update", "-qq"]))
        );
        assert_eq!(
            refresh("dnf", Elevation::Sudo),
            Some(cmd("sudo", ["dnf", "makecache", "-q"]))
        );
        assert_eq!(
            refresh("pacman", Elevation::Sudo),
            Some(cmd("sudo", ["pacman", "-Sy"]))
        );
        assert_eq!(
            refresh("zypper", Elevation::Sudo),
            Some(cmd("sudo", ["zypper", "-q", "refresh"]))
        );
        assert_eq!(
            refresh("apk", Elevation::Sudo),
            Some(cmd("sudo", ["apk", "update"]))
        );
        assert_eq!(refresh("unknown", Elevation::Sudo), None);
    }

    #[test]
    fn install_batches_packages_with_manager_specific_sudo_policy() {
        let packages = vec!["jq".to_owned(), "fd".to_owned()];

        assert_eq!(
            install("brew", &packages, Elevation::Sudo),
            Some(cmd("brew", ["install", "jq", "fd"]))
        );
        assert_eq!(
            install("apt", &packages, Elevation::Sudo),
            Some(cmd("sudo", ["apt-get", "install", "-y", "jq", "fd"]))
        );
        assert_eq!(
            install("dnf", &packages, Elevation::Sudo),
            Some(cmd("sudo", ["dnf", "install", "-y", "jq", "fd"]))
        );
        assert_eq!(
            install("pacman", &packages, Elevation::Sudo),
            Some(cmd(
                "sudo",
                ["pacman", "-Sy", "--needed", "--noconfirm", "jq", "fd"]
            ))
        );
        assert_eq!(
            install("zypper", &packages, Elevation::Sudo),
            Some(cmd("sudo", ["zypper", "-n", "install", "jq", "fd"]))
        );
        assert_eq!(
            install("apk", &packages, Elevation::Sudo),
            Some(cmd("sudo", ["apk", "add", "jq", "fd"]))
        );
    }

    #[test]
    fn install_refuses_empty_or_unknown_batches() {
        assert_eq!(install("apt", &[], Elevation::Sudo), None);
        assert_eq!(
            install("unknown", &["jq".to_owned()], Elevation::Sudo),
            None
        );
    }

    fn cmd(program: &str, args: impl IntoIterator<Item = impl Into<String>>) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}
