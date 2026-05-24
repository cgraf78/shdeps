//! Bash hook subprocess support.
//!
//! Custom dependency hooks are trusted user code, but the Rust port should not
//! source them into the Rust process or model them as ordinary libraries. Running
//! each hook query in a short Bash subprocess preserves the shell API boundary
//! while preventing hook-defined functions from leaking between dependencies.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Entry;
use crate::runtime::Roots;
use crate::status::CustomProbe;
use crate::Result;

const STATUS_SCRIPT: &str = r#"
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
. "$lib" 2>/dev/null || exit 1
. "$hook" 2>/dev/null || exit 1

declare -f exists >/dev/null 2>&1 || exit 1
exists "$name" >/dev/null 2>&1 || exit 1

if declare -f version >/dev/null 2>&1; then
  version "$name" 2>/dev/null || true
fi
"#;

const UNINSTALL_SCRIPT: &str = r#"
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
. "$lib" 2>/dev/null || exit 11
. "$hook" 2>/dev/null || exit 12

declare -f uninstall >/dev/null 2>&1 || exit 10
uninstall "$name"
"#;

const INSTALL_SCRIPT: &str = r#"
name=$1
lib=$2
hook=$3
reinstall=$4

unset -f exists version install post uninstall 2>/dev/null || true
. "$lib" 2>/dev/null || exit 11
. "$hook" 2>/dev/null || exit 12

declare -f exists >/dev/null 2>&1 || exit 10
if exists "$name" >/dev/null 2>&1 && [[ "$reinstall" != 1 ]]; then
  if declare -f version >/dev/null 2>&1; then
    version "$name" 2>/dev/null || true
  fi
  exit 20
fi

declare -f install >/dev/null 2>&1 || exit 13
install "$name" || exit 14
if declare -f version >/dev/null 2>&1; then
  version "$name" 2>/dev/null || true
fi
"#;

const POST_SCRIPT: &str = r#"
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
. "$lib" 2>/dev/null || exit 11
. "$hook" 2>/dev/null || exit 12

declare -f post >/dev/null 2>&1 || exit 10
post "$name"
"#;

/// Result of attempting an optional hook `uninstall(name)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstall {
    /// The hook file does not exist.
    MissingHook,
    /// The hook file exists but has no `uninstall` function.
    MissingFunction,
    /// The hook file or compatibility layer failed to source.
    SourceFailed,
    /// `uninstall(name)` exists but returned non-zero.
    Failed,
    /// `uninstall(name)` ran successfully.
    Removed,
}

/// Result of running `install(name)` for a custom dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// Hook file does not exist.
    MissingHook,
    /// Hook exists but cannot be used because a required function is missing.
    MissingFunction,
    /// Hook file or compatibility layer failed to source.
    SourceFailed,
    /// `exists(name)` reported the dependency was already installed.
    Already {
        /// Optional version output from `version(name)`.
        detail: String,
    },
    /// `install(name)` failed.
    Failed,
    /// `install(name)` ran successfully.
    Installed {
        /// Optional version output after install.
        detail: String,
    },
}

/// Result of running optional `post(name)` after a dependency changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Post {
    /// Hook file does not exist.
    MissingHook,
    /// Hook file exists but has no `post` function.
    MissingFunction,
    /// Hook file or compatibility layer failed to source.
    SourceFailed,
    /// `post(name)` returned non-zero.
    Failed,
    /// `post(name)` ran successfully.
    Ran,
}

/// Custom-status probe that evaluates hook `exists`/`version` in Bash.
#[derive(Debug, Clone)]
pub struct BashCustomProbe {
    shdeps_lib: PathBuf,
}

impl BashCustomProbe {
    /// Creates a probe using an explicit `shdeps.sh` compatibility layer.
    #[must_use]
    pub fn new(shdeps_lib: impl Into<PathBuf>) -> Self {
        Self {
            shdeps_lib: shdeps_lib.into(),
        }
    }

    /// Returns the configured compatibility-layer path.
    #[must_use]
    pub fn shdeps_lib(&self) -> &Path {
        &self.shdeps_lib
    }

    /// Runs the optional `uninstall(name)` hook for prune and method cleanup.
    pub fn uninstall(&self, name: &str, roots: &Roots) -> Result<Uninstall> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Uninstall::MissingHook);
        }
        if !self.shdeps_lib.is_file() {
            return Ok(Uninstall::SourceFailed);
        }

        let output = self
            .command(UNINSTALL_SCRIPT, name, &hook)
            .env("SHDEPS_CONF_DIR", &roots.conf_dir)
            .env("SHDEPS_HOOKS_DIR", &roots.hooks_dir)
            .env("SHDEPS_STATE_DIR", &roots.state_dir)
            .env("SHDEPS_GIT_DEV_DIR", &roots.git_dev_dir)
            .env("SHDEPS_INSTALL_DIR", &roots.install_dir)
            .env("SHDEPS_BIN_DIR", &roots.bin_dir)
            .env("SHDEPS_CURRENT_DEP", name)
            .env("SHDEPS_HOOK_PHASE", "uninstall")
            .output()?;

        Ok(match output.status.code() {
            Some(0) => Uninstall::Removed,
            Some(10) => Uninstall::MissingFunction,
            Some(11 | 12) => Uninstall::SourceFailed,
            _ => Uninstall::Failed,
        })
    }

    /// Runs `install(name)` for a custom dependency hook.
    pub fn install(&self, name: &str, roots: &Roots, reinstall: bool) -> Result<Install> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Install::MissingHook);
        }
        if !self.shdeps_lib.is_file() {
            return Ok(Install::SourceFailed);
        }

        let output = self
            .command(INSTALL_SCRIPT, name, &hook)
            .arg(if reinstall { "1" } else { "0" })
            .env("SHDEPS_CONF_DIR", &roots.conf_dir)
            .env("SHDEPS_HOOKS_DIR", &roots.hooks_dir)
            .env("SHDEPS_STATE_DIR", &roots.state_dir)
            .env("SHDEPS_GIT_DEV_DIR", &roots.git_dev_dir)
            .env("SHDEPS_INSTALL_DIR", &roots.install_dir)
            .env("SHDEPS_BIN_DIR", &roots.bin_dir)
            .env("SHDEPS_CURRENT_DEP", name)
            .env("SHDEPS_HOOK_PHASE", "install")
            .output()?;

        let detail = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(match output.status.code() {
            Some(0) => Install::Installed { detail },
            Some(20) => Install::Already { detail },
            Some(10 | 13) => Install::MissingFunction,
            Some(11 | 12) => Install::SourceFailed,
            _ => Install::Failed,
        })
    }

    /// Runs optional `post(name)` for a dependency that changed.
    pub fn post(&self, name: &str, roots: &Roots) -> Result<Post> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Post::MissingHook);
        }
        if !self.shdeps_lib.is_file() {
            return Ok(Post::SourceFailed);
        }

        let output = self
            .command(POST_SCRIPT, name, &hook)
            .env("SHDEPS_CONF_DIR", &roots.conf_dir)
            .env("SHDEPS_HOOKS_DIR", &roots.hooks_dir)
            .env("SHDEPS_STATE_DIR", &roots.state_dir)
            .env("SHDEPS_GIT_DEV_DIR", &roots.git_dev_dir)
            .env("SHDEPS_INSTALL_DIR", &roots.install_dir)
            .env("SHDEPS_BIN_DIR", &roots.bin_dir)
            .env("SHDEPS_CURRENT_DEP", name)
            .env("SHDEPS_HOOK_PHASE", "post")
            .output()?;

        Ok(match output.status.code() {
            Some(0) => Post::Ran,
            Some(10) => Post::MissingFunction,
            Some(11 | 12) => Post::SourceFailed,
            _ => Post::Failed,
        })
    }

    fn command(&self, script: &'static str, name: &str, hook: &Path) -> Command {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(script)
            .arg("shdeps-hook")
            .arg(name)
            .arg(&self.shdeps_lib)
            .arg(hook);
        command
    }
}

impl CustomProbe for BashCustomProbe {
    fn installed_detail(&self, entry: &Entry, roots: &Roots) -> Result<Option<String>> {
        let hook = roots.hooks_dir.join(format!("{}.sh", entry.name));
        if !hook.is_file() || !self.shdeps_lib.is_file() {
            return Ok(None);
        }

        let output = self
            .command(STATUS_SCRIPT, &entry.name, &hook)
            .env("SHDEPS_CONF_DIR", &roots.conf_dir)
            .env("SHDEPS_HOOKS_DIR", &roots.hooks_dir)
            .env("SHDEPS_STATE_DIR", &roots.state_dir)
            .env("SHDEPS_GIT_DEV_DIR", &roots.git_dev_dir)
            .env("SHDEPS_INSTALL_DIR", &roots.install_dir)
            .env("SHDEPS_BIN_DIR", &roots.bin_dir)
            .output()?;

        if !output.status.success() {
            return Ok(None);
        }

        let detail = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        if detail.is_empty() {
            Ok(Some(String::new()))
        } else {
            Ok(Some(detail))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{BashCustomProbe, Install, Post, Uninstall};
    use crate::config::parse_entry;
    use crate::runtime::Roots;
    use crate::status::CustomProbe;

    #[test]
    fn custom_probe_returns_version_when_exists_succeeds() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ "$1" == tool ]]; }
version() { printf '1.2.3\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let detail = probe
            .installed_detail(&parse_entry("tool|custom|-|-|-", None), &roots)
            .unwrap();

        assert_eq!(detail.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn custom_probe_treats_missing_hook_source_or_exists_as_missing() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("missing.sh"),
            "exists() { return 1; }\n",
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe
                .installed_detail(&parse_entry("missing|custom|-|-|-", None), &roots)
                .unwrap(),
            None
        );
        assert_eq!(
            probe
                .installed_detail(&parse_entry("no-hook|custom|-|-|-", None), &roots)
                .unwrap(),
            None
        );
    }

    #[test]
    fn custom_probe_sets_shdeps_root_environment_for_hooks() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("envtool.sh"),
            r#"
exists() { [[ "$SHDEPS_INSTALL_DIR" == */share ]]; }
version() { printf '%s\n' "$SHDEPS_BIN_DIR"; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let detail = probe
            .installed_detail(&parse_entry("envtool|custom|-|-|-", None), &roots)
            .unwrap();

        assert_eq!(detail.as_deref(), Some(roots.bin_dir.to_str().unwrap()));
    }

    #[test]
    fn uninstall_runs_optional_hook_with_context() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
uninstall() {
  [[ "$SHDEPS_CURRENT_DEP" == tool ]] || return 1
  [[ "$SHDEPS_HOOK_PHASE" == uninstall ]] || return 1
  printf '%s\n' "$1" > "$SHDEPS_STATE_DIR/uninstalled"
}
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.uninstall("tool", &roots).unwrap();

        assert_eq!(result, Uninstall::Removed);
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("uninstalled")).unwrap(),
            "tool\n"
        );
    }

    #[test]
    fn install_runs_custom_hook_and_reports_change() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/tool-installed" ]]; }
install() { printf '1\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
version() { printf '2.0.0\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.install("tool", &roots, false).unwrap();

        assert_eq!(
            result,
            Install::Installed {
                detail: "2.0.0".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("tool-installed")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn install_skips_existing_custom_hook_unless_reinstalling() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        fs::write(roots.state_dir.join("tool-installed"), "old\n").unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/tool-installed" ]]; }
install() { printf 'new\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
version() { printf 'old-version\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe.install("tool", &roots, false).unwrap(),
            Install::Already {
                detail: "old-version".to_owned()
            }
        );
        assert_eq!(
            probe.install("tool", &roots, true).unwrap(),
            Install::Installed {
                detail: "old-version".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("tool-installed")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn post_runs_optional_hook_with_context() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
post() { printf '%s:%s\n' "$1" "$SHDEPS_HOOK_PHASE" > "$SHDEPS_STATE_DIR/post-ran"; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.post("tool", &roots).unwrap();

        assert_eq!(result, Post::Ran);
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("post-ran")).unwrap(),
            "tool:post\n"
        );
    }

    #[test]
    fn uninstall_classifies_missing_and_failed_hooks() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(&roots.hooks_dir.join("no-fn.sh"), "exists() { :; }\n");
        write_hook(
            &roots.hooks_dir.join("fails.sh"),
            "uninstall() { return 7; }\n",
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe.uninstall("missing", &roots).unwrap(),
            Uninstall::MissingHook
        );
        assert_eq!(
            probe.uninstall("no-fn", &roots).unwrap(),
            Uninstall::MissingFunction
        );
        assert_eq!(probe.uninstall("fails", &roots).unwrap(), Uninstall::Failed);
        assert_eq!(
            BashCustomProbe::new(roots.home.join("missing-shdeps.sh"))
                .uninstall("fails", &roots)
                .unwrap(),
            Uninstall::SourceFailed
        );
    }

    fn write_hook(path: &PathBuf, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn roots() -> Roots {
        let root = temp_dir("hooks");
        Roots {
            conf_dir: root.join("config"),
            hooks_dir: root.join("config/hooks.d"),
            state_dir: root.join("state"),
            git_dev_dir: root.join("git"),
            install_dir: root.join("share"),
            bin_dir: root.join("bin"),
            home: root,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("shdeps-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
