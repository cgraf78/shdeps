//! Runtime path and machine identity resolution.
//!
//! The Bash CLI exports defaults before sourcing `shdeps.sh`; the Rust port
//! needs the same answers without making command modules re-learn environment
//! precedence. This module keeps those defaults centralized so cheap commands,
//! future `__api` bridges, cache keys, and install methods all agree on which
//! config/state/install roots and platform identity are active.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::dep_path;
use crate::platform::{self, RuntimeEnv};

/// Optional values supplied by CLI flags before environment defaults apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `-c/--config` path. A directory is used directly; a file uses its parent.
    pub config: Option<PathBuf>,
    /// `-f/--force` flag from the CLI.
    pub force: bool,
    /// `-R/--reinstall` flag from the CLI.
    pub reinstall: bool,
}

/// Runtime roots used by commands that need filesystem context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// CLI config directory.
    pub conf_dir: PathBuf,
    /// Hook directory, defaulting below the config directory.
    pub hooks_dir: PathBuf,
    /// State directory, defaulting below XDG state.
    pub state_dir: PathBuf,
    /// Local development clone root.
    pub git_dev_dir: PathBuf,
    /// Managed dependency install root.
    pub install_dir: PathBuf,
    /// Public binary link directory.
    pub bin_dir: PathBuf,
    /// Home directory used for legacy relative paths and defaults.
    pub home: PathBuf,
}

impl Roots {
    /// Converts to the narrower roots required by dependency path commands.
    #[must_use]
    pub fn dep_path_roots(&self) -> dep_path::Roots {
        dep_path::Roots {
            conf_dir: self.conf_dir.clone(),
            state_dir: self.state_dir.clone(),
            git_dev_dir: self.git_dev_dir.clone(),
            install_dir: self.install_dir.clone(),
        }
    }
}

/// Accessor for environment variables and host commands.
///
/// Tests provide deterministic values through this trait. Production uses the
/// process environment plus small host probes, but keeping that behind a trait
/// prevents command code from growing hidden dependencies on the developer's
/// actual machine.
pub trait Env {
    /// Returns an environment variable value.
    fn var_os(&self, name: &str) -> Option<OsString>;

    /// Returns command stdout when the probe succeeds.
    fn command_output(&self, command: &str, args: &[&str]) -> Option<String>;

    /// Returns file content for host-probe files.
    fn read_to_string(&self, path: &Path) -> Option<String>;
}

/// Real process environment and host probes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var_os(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    fn command_output(&self, command: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(command).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn read_to_string(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }
}

/// Resolves runtime roots from CLI overrides and environment defaults.
#[must_use]
pub fn roots(env: &impl Env, overrides: &Overrides) -> Roots {
    let home = env_path(env, "HOME").unwrap_or_else(|| PathBuf::from("."));
    let conf_dir = conf_dir(env, overrides, &home);
    let hooks_dir = env_path(env, "SHDEPS_HOOKS_DIR").unwrap_or_else(|| conf_dir.join("hooks.d"));
    let state_dir = env_path(env, "SHDEPS_STATE_DIR").unwrap_or_else(|| {
        env_path(env, "XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("shdeps")
    });

    Roots {
        conf_dir,
        hooks_dir,
        state_dir,
        git_dev_dir: env_path(env, "SHDEPS_GIT_DEV_DIR").unwrap_or_else(|| home.join("git")),
        install_dir: env_path(env, "SHDEPS_INSTALL_DIR")
            .unwrap_or_else(|| home.join(".local/share")),
        bin_dir: env_path(env, "SHDEPS_BIN_DIR").unwrap_or_else(|| home.join(".local/bin")),
        home,
    }
}

/// Resolves the platform/host identity used by filters and cache keys.
#[must_use]
pub fn runtime_env(env: &impl Env) -> RuntimeEnv {
    let platform = env_string(env, "SHDEPS_TEST_PLATFORM").unwrap_or_else(|| {
        let uname = env
            .command_output("uname", &["-s"])
            .unwrap_or_else(|| std::env::consts::OS.to_owned());
        let proc_version = env.read_to_string(Path::new("/proc/version"));
        platform::normalize_platform(uname.trim(), proc_version.as_deref())
    });
    let host = env_string(env, "SHDEPS_TEST_HOST").unwrap_or_else(|| {
        env.command_output("hostname", &["-s"])
            .or_else(|| env.command_output("hostname", &[]))
            // Bash command substitution produced an empty host when both
            // probes failed. That matters for legacy host-filter edge cases:
            // callers that build a spec from the same failed probe can produce
            // `hosts=!`, and the Bash matcher treated that as excluding the
            // current empty host. Avoid inventing "unknown" here or child
            // `__api host-match` calls stop matching the caller's shell view
            // in minimal containers where `hostname -s` is unavailable.
            .unwrap_or_default()
    });

    RuntimeEnv::new(platform, host.trim()).with_android(is_android(env))
}

/// Returns whether the process is running directly on Android.
///
/// Android reports a Linux kernel through `uname -s`, so release selection
/// needs a separate Bionic-runtime signal. Environment variables are the cheap
/// and reliable Termux path; `uname -o` keeps the detection useful in other
/// Android terminal environments.
#[must_use]
pub fn is_android(env: &impl Env) -> bool {
    env.var_os("ANDROID_ROOT")
        .is_some_and(|value| !value.is_empty())
        || env
            .var_os("TERMUX_VERSION")
            .is_some_and(|value| !value.is_empty())
        || env
            .command_output("uname", &["-o"])
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("android"))
}

/// Returns whether force mode is active.
#[must_use]
pub fn force(env: &impl Env, overrides: &Overrides) -> bool {
    overrides.force || overrides.reinstall || env_flag(env, "SHDEPS_FORCE")
}

/// Returns whether reinstall mode is active.
#[must_use]
pub fn reinstall(env: &impl Env, overrides: &Overrides) -> bool {
    overrides.reinstall || env_flag(env, "SHDEPS_REINSTALL")
}

/// Returns the cached package manager without probing the host.
///
/// The sourceable wrapper reads the package manager only after another path has
/// already detected it. The wrapper/prelude exports this value when it has one;
/// absent export means "unknown yet", not "go detect it now".
#[must_use]
pub fn pkg_mgr(env: &impl Env) -> String {
    env_string(env, "SHDEPS_PKG_MGR").unwrap_or_default()
}

fn env_flag(env: &impl Env, name: &str) -> bool {
    matches!(env_string(env, name).as_deref(), Some("1"))
}

fn conf_dir(env: &impl Env, overrides: &Overrides, home: &Path) -> PathBuf {
    if let Some(path) = &overrides.config {
        return if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
    }

    env_path(env, "SHDEPS_CONF_DIR").unwrap_or_else(|| {
        env_path(env, "XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("shdeps")
    })
}

fn env_path(env: &impl Env, name: &str) -> Option<PathBuf> {
    env.var_os(name)
        .filter(|value| !value.as_os_str().is_empty())
        .map(PathBuf::from)
}

fn env_string(env: &impl Env, name: &str) -> Option<String> {
    env.var_os(name)
        .filter(|value| !value.as_os_str().is_empty())
        .and_then(|value| value.into_string().ok())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{Env, Overrides, roots, runtime_env};

    #[test]
    fn defaults_follow_cli_environment_contract() {
        let env = FakeEnv::new().with_var("HOME", "/home/tester");

        let roots = roots(&env, &Overrides::default());

        assert_eq!(roots.conf_dir, PathBuf::from("/home/tester/.config/shdeps"));
        assert_eq!(
            roots.hooks_dir,
            PathBuf::from("/home/tester/.config/shdeps/hooks.d")
        );
        assert_eq!(
            roots.state_dir,
            PathBuf::from("/home/tester/.local/state/shdeps")
        );
        assert_eq!(roots.git_dev_dir, PathBuf::from("/home/tester/git"));
        assert_eq!(
            roots.install_dir,
            PathBuf::from("/home/tester/.local/share")
        );
        assert_eq!(roots.bin_dir, PathBuf::from("/home/tester/.local/bin"));
    }

    #[test]
    fn config_file_override_uses_parent_directory() {
        let env = FakeEnv::new().with_var("HOME", "/home/tester");
        let overrides = Overrides {
            config: Some(PathBuf::from("/tmp/app/deps.conf")),
            ..Overrides::default()
        };

        assert_eq!(roots(&env, &overrides).conf_dir, PathBuf::from("/tmp/app"));
    }

    #[test]
    fn environment_overrides_roots_without_recomputing_dependents() {
        let env = FakeEnv::new()
            .with_var("HOME", "/home/tester")
            .with_var("SHDEPS_CONF_DIR", "/cfg")
            .with_var("SHDEPS_HOOKS_DIR", "/hooks")
            .with_var("SHDEPS_STATE_DIR", "/state")
            .with_var("SHDEPS_GIT_DEV_DIR", "/git")
            .with_var("SHDEPS_INSTALL_DIR", "/install")
            .with_var("SHDEPS_BIN_DIR", "/bin");

        let roots = roots(&env, &Overrides::default());

        assert_eq!(roots.conf_dir, PathBuf::from("/cfg"));
        assert_eq!(roots.hooks_dir, PathBuf::from("/hooks"));
        assert_eq!(roots.state_dir, PathBuf::from("/state"));
        assert_eq!(roots.git_dev_dir, PathBuf::from("/git"));
        assert_eq!(roots.install_dir, PathBuf::from("/install"));
        assert_eq!(roots.bin_dir, PathBuf::from("/bin"));
    }

    #[test]
    fn runtime_identity_prefers_test_overrides() {
        let env = FakeEnv::new()
            .with_var("SHDEPS_TEST_PLATFORM", "wsl")
            .with_var("SHDEPS_TEST_HOST", "BUILD-HOST");

        let runtime = runtime_env(&env);

        assert_eq!(runtime.platform(), "wsl");
        assert_eq!(runtime.host(), "build-host");
    }

    #[test]
    fn runtime_identity_uses_host_probes_when_not_overridden() {
        let env = FakeEnv::new()
            .with_command("uname -s", "Darwin\n")
            .with_command("hostname -s", "MacBook\n");

        let runtime = runtime_env(&env);

        assert_eq!(runtime.platform(), "macos");
        assert_eq!(runtime.host(), "macbook");
    }

    #[test]
    fn runtime_identity_carries_termux_signal_when_uname_o_is_unavailable() {
        let env = FakeEnv::new()
            .with_var("TERMUX_VERSION", "0.118.3")
            .with_command("uname -s", "Linux\n")
            .with_command("hostname -s", "phone\n");

        let runtime = runtime_env(&env);

        assert_eq!(runtime.platform(), "linux");
        assert!(runtime.is_android());
    }

    #[test]
    fn runtime_identity_preserves_legacy_empty_host_when_probes_fail() {
        let env = FakeEnv::new().with_command("uname -s", "Linux\n");

        let runtime = runtime_env(&env);

        assert_eq!(runtime.platform(), "linux");
        assert_eq!(runtime.host(), "");
    }

    #[test]
    fn android_identity_uses_termux_and_kernel_signals() {
        assert!(super::is_android(
            &FakeEnv::new().with_var("TERMUX_VERSION", "0.118.3")
        ));
        assert!(super::is_android(
            &FakeEnv::new().with_var("ANDROID_ROOT", "/system")
        ));
        assert!(super::is_android(
            &FakeEnv::new().with_command("uname -o", "Android\n")
        ));
        assert!(!super::is_android(
            &FakeEnv::new().with_command("uname -o", "GNU/Linux\n")
        ));
        assert!(!super::is_android(
            &FakeEnv::new()
                .with_var("ANDROID_ROOT", "")
                .with_var("TERMUX_VERSION", "")
        ));
    }

    #[test]
    fn mode_flags_preserve_cli_precedence_without_package_detection() {
        let env = FakeEnv::new()
            .with_var("SHDEPS_FORCE", "0")
            .with_var("SHDEPS_REINSTALL", "0")
            .with_var("SHDEPS_PKG_MGR", "apt");
        let overrides = Overrides {
            config: None,
            force: false,
            reinstall: true,
        };

        assert!(super::force(&env, &overrides));
        assert!(super::reinstall(&env, &overrides));
        assert_eq!(super::pkg_mgr(&env), "apt");
    }

    #[derive(Debug, Default)]
    struct FakeEnv {
        vars: BTreeMap<String, OsString>,
        commands: BTreeMap<String, String>,
        files: BTreeMap<PathBuf, String>,
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
            let joined = if args.is_empty() {
                command.to_owned()
            } else {
                format!("{command} {}", args.join(" "))
            };
            self.commands.get(&joined).cloned()
        }

        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }
    }
}
