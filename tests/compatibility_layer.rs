//! Custom hooks sourced through the real `shdeps.sh` compatibility layer.
//!
//! Sourcing the layer performs the wrapper ABI handshake with a Rust binary,
//! so these checks live where Cargo builds that binary for the current source
//! tree. A unit test could only find whichever `target/debug/shdeps` happened
//! to exist: missing under `cargo test --lib` or a separate target directory,
//! and stale after source changes.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use shdeps::hooks::{BashCustomProbe, Install};
use shdeps::runtime::Roots;

/// A release-archive layout: `shdeps.sh` beside the binary it resolves.
struct Fixture {
    root: PathBuf,
    roots: Roots,
    library: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "shdeps-compatibility-layer-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let install = root.join("install");
        fs::create_dir_all(&install).unwrap();
        // The wrapper resolves its sibling binary through the directory, so
        // symlinked files exercise the same lookup as an unpacked archive.
        let library = install.join("shdeps.sh");
        symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("shdeps.sh"),
            &library,
        )
        .unwrap();
        symlink(env!("CARGO_BIN_EXE_shdeps"), install.join("shdeps")).unwrap();
        let roots = Roots {
            conf_dir: root.join("config"),
            hooks_dir: root.join("config/hooks.d"),
            state_dir: root.join("state"),
            git_dev_dir: root.join("git"),
            install_dir: root.join("share"),
            bin_dir: root.join("bin"),
            home: root.clone(),
        };
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        Self {
            root,
            roots,
            library,
        }
    }

    fn install(&self, hook: &str) -> Install {
        let path = self.roots.hooks_dir.join("tool.sh");
        fs::write(&path, hook).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        BashCustomProbe::new(&self.library)
            .install("tool", &self.roots, false)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn compatibility_bash_supported() -> bool {
    Command::new("bash")
        .args([
            "-c",
            "((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3)))",
        ])
        .status()
        .unwrap()
        .success()
}

// One sequential test: the Bash version probe is a plain child of this
// process, and a concurrently running hook boundary could otherwise see it as
// an unattributed adoptee.
#[test]
fn compatibility_layer_serves_custom_hooks() {
    assert!(
        std::env::var_os("SHDEPS_RUST_CLI").is_none(),
        "unset SHDEPS_RUST_CLI: the compatibility layer would prefer it over \
         the binary built for this source tree"
    );
    let supported = compatibility_bash_supported();

    // A safe install-failure warning reaches the parent as the failure detail.
    let fixture = Fixture::new("warning");
    let result = fixture.install(
        r#"
exists() { return 1; }
install() {
  shdeps_warn 'php-cs-fixer asset download failed'
  return 42
}
"#,
    );
    if supported {
        assert_eq!(
            result,
            Install::Failed {
                detail: "php-cs-fixer asset download failed".to_owned()
            }
        );
    } else {
        assert_eq!(result, Install::SourceFailed);
    }

    // `shdeps_curl` applies the bounded transfer policy before caller flags.
    let fixture = Fixture::new("curl");
    let result = fixture.install(
        r#"
exists() { return 1; }
curl() { printf '%s\n' "$@" > "$SHDEPS_STATE_DIR/curl-args"; }
install() {
  shdeps_curl -fsSL --no-netrc https://example.invalid/tool.tar.gz -o /dev/null
}
"#,
    );
    if !supported {
        assert_eq!(result, Install::SourceFailed);
        return;
    }
    assert_eq!(
        result,
        Install::Installed {
            detail: String::new()
        }
    );
    assert_eq!(
        fs::read_to_string(fixture.roots.state_dir.join("curl-args")).unwrap(),
        concat!(
            "--connect-timeout\n10\n",
            "--speed-limit\n1024\n",
            "--speed-time\n60\n",
            "--retry\n3\n",
            "-fsSL\n--no-netrc\n",
            "https://example.invalid/tool.tar.gz\n",
            "-o\n/dev/null\n"
        )
    );
}
