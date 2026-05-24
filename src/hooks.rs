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
}

impl CustomProbe for BashCustomProbe {
    fn installed_detail(&self, entry: &Entry, roots: &Roots) -> Result<Option<String>> {
        let hook = roots.hooks_dir.join(format!("{}.sh", entry.name));
        if !hook.is_file() || !self.shdeps_lib.is_file() {
            return Ok(None);
        }

        let output = Command::new("bash")
            .arg("-c")
            .arg(STATUS_SCRIPT)
            .arg("shdeps-custom-status")
            .arg(&entry.name)
            .arg(&self.shdeps_lib)
            .arg(&hook)
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

    use super::BashCustomProbe;
    use crate::config::parse_entry;
    use crate::runtime::Roots;
    use crate::status::CustomProbe;

    #[test]
    fn custom_probe_returns_version_when_exists_succeeds() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
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

    fn write_hook(path: &PathBuf, content: &str) {
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
