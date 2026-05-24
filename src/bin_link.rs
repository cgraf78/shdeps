//! Public command symlink helpers.
//!
//! Several install methods produce a binary under a managed install root and
//! expose it through `SHDEPS_BIN_DIR`. The ownership rule is subtle: shdeps may
//! replace its own symlink, but it must not overwrite a regular file the user
//! placed in that command path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;

/// Result of trying to expose one binary in the public bin directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// The source binary was missing or not executable, so no link was made.
    MissingSource,
    /// A non-symlink already exists at the public command path and was preserved.
    Preserved(PathBuf),
    /// The public symlink was created or replaced.
    Linked(PathBuf),
}

/// Links an executable source binary into `bin_dir` as `cmd`.
pub fn one(bin_dir: &Path, cmd: &str, source: &Path) -> Result<Link> {
    if !crate::process::executable_path(source) {
        return Ok(Link::MissingSource);
    }

    fs::create_dir_all(bin_dir)?;
    let target = bin_dir.join(cmd);
    if target.exists() && !target.is_symlink() {
        return Ok(Link::Preserved(target));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        symlink(source, &target)?;
    }

    Ok(Link::Linked(target))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::{one, Link};

    #[test]
    #[cfg(unix)]
    fn link_creates_public_symlink_for_executable_source() {
        let dir = temp_dir("create");
        let source = dir.join("share/tool/bin/tool");
        write_executable(&source);

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Linked(dir.join("bin/tool")));
        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
    }

    #[test]
    #[cfg(unix)]
    fn link_replaces_existing_symlink() {
        let dir = temp_dir("replace");
        let old = dir.join("old");
        let source = dir.join("new");
        write_executable(&old);
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        std::os::unix::fs::symlink(&old, dir.join("bin/tool")).unwrap();

        one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
    }

    #[test]
    fn link_preserves_regular_file_command() {
        let dir = temp_dir("preserve");
        let source = dir.join("source");
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/tool"), "user-owned").unwrap();

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Preserved(dir.join("bin/tool")));
        assert_eq!(
            fs::read_to_string(dir.join("bin/tool")).unwrap(),
            "user-owned"
        );
    }

    #[test]
    fn link_skips_missing_or_non_executable_source() {
        let dir = temp_dir("missing");
        let source = dir.join("source");

        assert_eq!(
            one(&dir.join("bin"), "tool", &source).unwrap(),
            Link::MissingSource
        );

        fs::write(&source, "not executable").unwrap();
        assert_eq!(
            one(&dir.join("bin"), "tool", &source).unwrap(),
            Link::MissingSource
        );
    }

    fn write_executable(path: &PathBuf) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-bin-link-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
