//! Install helpers for `github:release` assets.
//!
//! This module owns the filesystem side of release installs. It is deliberately
//! separate from GitHub fetching and asset selection so tests can pin down
//! compatibility-sensitive ownership behavior without constructing fake HTTP
//! clients. The first supported asset shape is the Bash fast path: a raw
//! standalone binary downloaded directly into the public bin directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;

/// Installs a raw standalone release binary into `SHDEPS_BIN_DIR`.
pub fn install_plain(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let target = bin_dir.join(cmd);
    let tmp = temp_path(&target);

    fs::write(&tmp, bytes)?;
    make_executable(&tmp)?;

    // `github:release` is the historical exception to shdeps' normal "never
    // overwrite regular files in SHDEPS_BIN_DIR" rule. Bash downloads/moves the
    // selected asset directly to the requested bin path, so preserve that
    // replacement behavior here and keep it isolated from the safer symlink
    // helpers used by repo, cargo, go, uv, and npm installs.
    fs::rename(&tmp, &target)?;
    Ok(target)
}

fn temp_path(target: &Path) -> PathBuf {
    let mut tmp = target.to_path_buf();
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("release-bin");
    tmp.set_file_name(format!(".{name}.tmp.{}", std::process::id()));
    tmp
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    #[test]
    fn plain_install_writes_executable_binary() {
        let dir = temp_dir("plain");

        let path = super::install_plain(&dir.join("bin"), "tool", b"binary").unwrap();

        assert_eq!(path, dir.join("bin/tool"));
        assert_eq!(fs::read(&path).unwrap(), b"binary");
        #[cfg(unix)]
        assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0);
    }

    #[test]
    fn plain_install_replaces_existing_regular_file_for_bash_compatibility() {
        let dir = temp_dir("replace");
        let target = dir.join("bin/tool");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"user-owned").unwrap();

        super::install_plain(&dir.join("bin"), "tool", b"release").unwrap();

        assert_eq!(fs::read(target).unwrap(), b"release");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-release-install-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
