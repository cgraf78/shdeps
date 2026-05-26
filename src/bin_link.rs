//! Public command symlink helpers.
//!
//! Several install methods produce a binary under a managed install root and
//! expose it through `SHDEPS_BIN_DIR`. The ownership rule is subtle: shdeps may
//! replace its own symlink, but it must not overwrite a regular file the user
//! placed in that command path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::link_state::{self, Kind};

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
///
/// Uses the shared `extras::replace_symlink` helper for the actual
/// staging+atomic-rename so the public-bin path and the man-page /
/// completion paths cannot drift in their TOCTOU semantics. Without
/// this consolidation the public-bin link used a wider
/// `remove_file` → `symlink` window in which the path was momentarily
/// missing entirely; under the state lock no shdeps↔shdeps race could
/// observe that window, but an external tool concurrently writing
/// into `~/.local/bin` could. The shared helper closes that gap.
pub fn one(bin_dir: &Path, cmd: &str, source: &Path) -> Result<Link> {
    if !crate::process::executable_path(source) {
        return Ok(Link::MissingSource);
    }

    fs::create_dir_all(bin_dir)?;
    let target = bin_dir.join(cmd);

    #[cfg(unix)]
    {
        if crate::extras::replace_symlink(source, &target)? {
            return Ok(Link::Linked(target));
        }
        return Ok(Link::Preserved(target));
    }
    #[cfg(not(unix))]
    {
        // Non-Unix targets are not a supported install platform; keep
        // the previous behavior of preserving any existing entry so
        // a stray Windows debug build does not stomp user files.
        if target.exists() {
            return Ok(Link::Preserved(target));
        }
        Ok(Link::Linked(target))
    }
}

/// Links every executable directly under `install_dir/bin`.
///
/// Repo dependencies may expose several public commands. Tracking the created
/// symlinks lets a later update remove stale commands when the repo changes
/// its bin directory contents.
pub fn from_dir(
    state_dir: &Path,
    bin_dir: &Path,
    name: &str,
    install_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let state_path = link_state::path(state_dir, name, Kind::Bin);
    link_state::unlink_tracked(&state_path)?;

    let source_dir = install_dir.join("bin");
    let Ok(entries) = fs::read_dir(&source_dir) else {
        return Ok(Vec::new());
    };

    let mut created = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !crate::process::executable_path(&path) {
            continue;
        }
        let Some(cmd) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Link::Linked(link) = one(bin_dir, cmd, &path)? {
            created.push(link);
        }
    }

    link_state::write(&state_path, &created)?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::{Link, one};

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

    #[test]
    #[cfg(unix)]
    fn link_replaces_dangling_symlink_via_atomic_rename() {
        // Regression for the bin_link / extras TOCTOU asymmetry. A
        // dangling symlink at the target path must still be treated
        // as shdeps-owned and replaced (not preserved as a stranger
        // file), and the replacement must succeed via the shared
        // staging+rename helper rather than the older delete-then-
        // create path. The observable invariant: at no point does
        // the target path become absent during replacement — a
        // `symlink_metadata` probe taken between the old and new
        // links would observe one of them, never NotFound.
        let dir = temp_dir("atomic-rename");
        let source = dir.join("real-source");
        let bogus = dir.join("does-not-exist");
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        // Stage a dangling symlink that points at a non-existent path.
        std::os::unix::fs::symlink(&bogus, dir.join("bin/tool")).unwrap();
        assert!(fs::symlink_metadata(dir.join("bin/tool")).is_ok());
        assert!(fs::metadata(dir.join("bin/tool")).is_err()); // dangling

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Linked(dir.join("bin/tool")));
        // The new symlink resolves and the dangling one is gone.
        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
        // And no `.tool.shdeps-link.<pid>.<stamp>` staging file got
        // left behind in the parent — staging-rename's success path
        // moves the staging entry to the target, not abandons it.
        let leftover_staging: Vec<_> = fs::read_dir(dir.join("bin"))
            .unwrap()
            .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
            .filter(|n| n.starts_with(".tool.shdeps-link."))
            .collect();
        assert!(
            leftover_staging.is_empty(),
            "no staging file should remain after successful rename"
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_from_dir_tracks_multiple_repo_commands_and_removes_stale_links() {
        let dir = temp_dir("from-dir");
        let install = dir.join("share/repo");
        let bin_dir = dir.join("bin");
        write_executable(&install.join("bin/tool-a"));
        write_executable(&install.join("bin/tool-b"));
        write_executable(&install.join("bin/not-direct/nested"));

        let created =
            super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

        assert_eq!(created.len(), 2);
        assert_eq!(
            fs::read_link(bin_dir.join("tool-a")).unwrap(),
            install.join("bin/tool-a")
        );
        assert_eq!(
            fs::read_link(bin_dir.join("tool-b")).unwrap(),
            install.join("bin/tool-b")
        );

        fs::remove_file(install.join("bin/tool-b")).unwrap();
        let created =
            super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

        assert_eq!(created, [bin_dir.join("tool-a")]);
        assert!(fs::symlink_metadata(bin_dir.join("tool-b")).is_err());
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
