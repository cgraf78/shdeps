//! Tracked symlink state for public command and extras links.
//!
//! `shdeps` writes `.binlinks` and `.links` files so relink, prune, and method
//! transitions can remove stale symlinks later. The state file lists paths that
//! shdeps created; cleanup still checks that each path is a symlink before
//! removing it so a user-owned regular file at the same location is preserved.

use std::fs;
use std::path::{Path, PathBuf};

use crate::state;
use crate::Result;

/// Link-state file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Public command symlinks under `SHDEPS_BIN_DIR`.
    Bin,
    /// Extra symlinks such as man pages and shell completions.
    Extras,
}

impl Kind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Bin => "binlinks",
            Self::Extras => "links",
        }
    }
}

/// Returns the link-state path for a dependency name.
#[must_use]
pub fn path(state_dir: &Path, name: &str, kind: Kind) -> PathBuf {
    state_dir.join(format!("{name}.{}", kind.suffix()))
}

/// Reads tracked links. Missing link-state files are empty state.
pub fn read(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Writes tracked links, removing the state file when there is nothing to track.
pub fn write(path: &Path, links: &[PathBuf]) -> Result<()> {
    if links.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }

    let mut content = String::new();
    for link in links {
        content.push_str(&link.to_string_lossy());
        content.push('\n');
    }
    state::write_atomic(path, &content)
}

/// Removes symlinks listed in a link-state file, then removes the state file.
pub fn unlink_tracked(path: &Path) -> Result<()> {
    for link in read(path)? {
        // `symlink_metadata` observes dangling symlinks too. That is important
        // for cleanup after a target install directory has already been
        // removed; `metadata` would follow the link, fail, and leave stale
        // public paths behind.
        if fs::symlink_metadata(&link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            fs::remove_file(link)?;
        }
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{path, read, unlink_tracked, write, Kind};

    #[test]
    fn paths_preserve_nested_dependency_names() {
        let state = PathBuf::from("/tmp/state");

        assert_eq!(
            path(&state, "owner/tool", Kind::Bin),
            PathBuf::from("/tmp/state/owner/tool.binlinks")
        );
        assert_eq!(
            path(&state, "owner/tool", Kind::Extras),
            PathBuf::from("/tmp/state/owner/tool.links")
        );
    }

    #[test]
    fn read_missing_state_as_empty() {
        let dir = temp_dir("missing");

        assert_eq!(
            read(&dir.join("missing.links")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn write_and_read_link_state() {
        let dir = temp_dir("round-trip");
        let state = path(&dir, "owner/tool", Kind::Extras);
        let links = [dir.join("man/man1/tool.1"), dir.join("zsh/_tool")];

        write(&state, &links).unwrap();

        assert_eq!(read(&state).unwrap(), links);
    }

    #[test]
    fn write_empty_removes_state_file() {
        let dir = temp_dir("empty");
        let state = path(&dir, "tool", Kind::Bin);

        write(&state, &[dir.join("bin/tool")]).unwrap();
        write(&state, &[]).unwrap();

        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn unlink_tracked_removes_symlinks_and_preserves_regular_files() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("unlink");
        let state = path(&dir, "tool", Kind::Bin);
        let target = dir.join("target");
        let symlink_path = dir.join("bin/tool");
        let regular_path = dir.join("bin/regular-tool");
        fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
        fs::write(&target, "target").unwrap();
        symlink(&target, &symlink_path).unwrap();
        fs::write(&regular_path, "user-owned").unwrap();

        write(&state, &[symlink_path.clone(), regular_path.clone()]).unwrap();
        unlink_tracked(&state).unwrap();

        assert!(!symlink_path.exists());
        assert_eq!(fs::read_to_string(&regular_path).unwrap(), "user-owned");
        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn unlink_tracked_removes_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("dangling");
        let state = path(&dir, "tool", Kind::Extras);
        let target = dir.join("missing-target");
        let symlink_path = dir.join("share/man/man1/tool.1");
        fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
        symlink(&target, &symlink_path).unwrap();

        write(&state, std::slice::from_ref(&symlink_path)).unwrap();
        unlink_tracked(&state).unwrap();

        assert!(fs::symlink_metadata(&symlink_path).is_err());
        assert!(!state.exists());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-link-state-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
