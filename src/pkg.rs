//! Package-manager command planning.
//!
//! `pkg` dependencies are the only install method that can cross into
//! system-owned state. Keep the manager-specific command shapes here so update,
//! custom hook helpers, and future dry-run diagnostics cannot drift into subtly
//! different sudo/install behavior.

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

/// Builds the metadata refresh command for managers with mutable repo caches.
#[must_use]
pub fn refresh(mgr: &str) -> Option<CommandSpec> {
    match mgr {
        // Homebrew does its own metadata management around install/info. Bash
        // also skips an explicit brew refresh, which keeps normal macOS update
        // runs from paying a surprising `brew update` cost.
        "brew" => None,
        "apt" => Some(CommandSpec::new("sudo", ["apt-get", "update", "-qq"])),
        "dnf" => Some(CommandSpec::new("sudo", ["dnf", "makecache", "-q"])),
        "pacman" => Some(CommandSpec::new("sudo", ["pacman", "-Sy"])),
        "zypper" => Some(CommandSpec::new("sudo", ["zypper", "-q", "refresh"])),
        "apk" => Some(CommandSpec::new("sudo", ["apk", "update"])),
        _ => None,
    }
}

/// Builds the batched install command for one or more packages.
///
/// The Rust updater should preserve batching because package-manager startup
/// dominates cold installs. Callers should pass only packages that survived
/// availability checks; an empty package list intentionally returns `None`.
#[must_use]
pub fn install(mgr: &str, packages: &[String]) -> Option<CommandSpec> {
    if packages.is_empty() {
        return None;
    }

    let mut args = match mgr {
        "brew" => vec!["install".to_owned()],
        "apt" => vec!["apt-get".to_owned(), "install".to_owned(), "-y".to_owned()],
        "dnf" => vec!["dnf".to_owned(), "install".to_owned(), "-y".to_owned()],
        "pacman" => vec![
            "pacman".to_owned(),
            "-Sy".to_owned(),
            "--needed".to_owned(),
            "--noconfirm".to_owned(),
        ],
        "zypper" => vec!["zypper".to_owned(), "-n".to_owned(), "install".to_owned()],
        "apk" => vec!["apk".to_owned(), "add".to_owned()],
        _ => return None,
    };
    args.extend(packages.iter().cloned());

    if mgr == "brew" {
        Some(CommandSpec::new("brew", args))
    } else {
        Some(CommandSpec::new("sudo", args))
    }
}

#[cfg(test)]
mod tests {
    use super::{available, install, refresh, CommandSpec};

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
    fn refresh_skips_brew_and_uses_quiet_metadata_commands() {
        assert_eq!(refresh("brew"), None);
        assert_eq!(
            refresh("apt"),
            Some(cmd("sudo", ["apt-get", "update", "-qq"]))
        );
        assert_eq!(
            refresh("dnf"),
            Some(cmd("sudo", ["dnf", "makecache", "-q"]))
        );
        assert_eq!(refresh("pacman"), Some(cmd("sudo", ["pacman", "-Sy"])));
        assert_eq!(
            refresh("zypper"),
            Some(cmd("sudo", ["zypper", "-q", "refresh"]))
        );
        assert_eq!(refresh("apk"), Some(cmd("sudo", ["apk", "update"])));
        assert_eq!(refresh("unknown"), None);
    }

    #[test]
    fn install_batches_packages_with_manager_specific_sudo_policy() {
        let packages = vec!["jq".to_owned(), "fd".to_owned()];

        assert_eq!(
            install("brew", &packages),
            Some(cmd("brew", ["install", "jq", "fd"]))
        );
        assert_eq!(
            install("apt", &packages),
            Some(cmd("sudo", ["apt-get", "install", "-y", "jq", "fd"]))
        );
        assert_eq!(
            install("dnf", &packages),
            Some(cmd("sudo", ["dnf", "install", "-y", "jq", "fd"]))
        );
        assert_eq!(
            install("pacman", &packages),
            Some(cmd(
                "sudo",
                ["pacman", "-Sy", "--needed", "--noconfirm", "jq", "fd"]
            ))
        );
        assert_eq!(
            install("zypper", &packages),
            Some(cmd("sudo", ["zypper", "-n", "install", "jq", "fd"]))
        );
        assert_eq!(
            install("apk", &packages),
            Some(cmd("sudo", ["apk", "add", "jq", "fd"]))
        );
    }

    #[test]
    fn install_refuses_empty_or_unknown_batches() {
        assert_eq!(install("apt", &[]), None);
        assert_eq!(install("unknown", &["jq".to_owned()]), None);
    }

    fn cmd(program: &str, args: impl IntoIterator<Item = impl Into<String>>) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}
