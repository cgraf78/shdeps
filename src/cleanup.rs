//! Ownership-aware cleanup for prune and method transitions.
//!
//! Bash historically had separate call paths for orphan pruning and method
//! transitions, but both paths make the same ownership decision: remove only
//! artifacts that shdeps created or explicitly tracks, and leave system
//! packages, local development clones, and user-owned files alone. Keeping the
//! built-in cleanup rules here gives the Rust updater one place to preserve
//! those safety boundaries.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::link_state::{self, Kind};
use crate::manifest::{Manifest, ManifestEntry};
use crate::Result;

/// Filesystem roots needed for built-in artifact cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// State directory containing stamps, `.links`, and `.binlinks`.
    pub state_dir: PathBuf,
    /// Managed install root, normally `~/.local/share`.
    pub install_dir: PathBuf,
    /// Public command directory, normally `~/.local/bin`.
    pub bin_dir: PathBuf,
    /// Home directory used to interpret legacy relative manifest paths.
    pub home: PathBuf,
}

/// Summary of built-in cleanup decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Paths removed by the built-in cleanup rules.
    pub removed: Vec<PathBuf>,
    /// True when a package-manager dependency was intentionally preserved.
    pub preserved_package: bool,
    /// True when a custom dependency needs hook/manual cleanup outside this layer.
    pub custom_requires_hook: bool,
}

impl Summary {
    fn note_removed(&mut self, path: impl Into<PathBuf>) {
        self.removed.push(path.into());
    }
}

/// Returns manifest entries whose installed method differs from current config.
#[must_use]
pub fn method_transitions(
    manifest: &Manifest,
    config: &[crate::config::Entry],
) -> Vec<ManifestEntry> {
    config
        .iter()
        .filter_map(|entry| {
            let installed = manifest.get(&entry.name)?;
            (installed.method != entry.method).then(|| installed.clone())
        })
        .collect()
}

/// Removes built-in artifacts for one manifest entry.
///
/// Hook `uninstall()` execution intentionally lives outside this function. Hooks
/// can run arbitrary shell and must not inherit the broad state lock; this layer
/// is the deterministic filesystem cleanup that runs before/after hook handling
/// depending on the higher-level prune or transition flow.
pub fn remove_builtin(entry: &ManifestEntry, roots: &Roots) -> Result<Summary> {
    let mut summary = Summary::default();

    match entry.method.as_str() {
        "pkg" => {
            // System packages are explicitly not owned by shdeps. Migrating a
            // dependency from `pkg` to another method must clear shdeps tracking
            // later, but uninstalling the OS package would be surprising and
            // potentially destructive.
            summary.preserved_package = true;
        }
        "github:repo" => {
            unlink_state(roots, &entry.name, Kind::Bin, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, &mut summary)?;

            if !entry.install_path.is_empty() {
                let install_path = manifest_path(&entry.install_path, &roots.home);
                remove_any(&install_path, &mut summary)?;
            }

            remove_any(
                &roots.bin_dir.join(config::short_name(&entry.name)),
                &mut summary,
            )?;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        "github:release" | "cargo" | "go" | "uv" | "npm" => {
            unlink_state(roots, &entry.name, Kind::Extras, &mut summary)?;
            remove_any(&roots.bin_dir.join(&entry.cmd), &mut summary)?;

            let install_root = roots.install_dir.join(&entry.name);
            remove_any(&install_root, &mut summary)?;
            remove_empty_install_parents(&install_root, &roots.install_dir, &mut summary)?;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        "custom" => {
            // Custom deps have no built-in ownership model. The hook runner owns
            // `uninstall()`; this function only clears shdeps' own stamps so a
            // future reinstall does not inherit stale remote/cache state.
            summary.custom_requires_hook = true;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        _ => {}
    }

    Ok(summary)
}

/// Removes TTL and revision stamps for a dependency name.
pub fn remove_stamps(state_dir: &Path, name: &str, summary: &mut Summary) -> Result<()> {
    let stamp_dir = state_dir
        .join(name)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.to_path_buf());
    let Some(base_name) = name.rsplit('/').next() else {
        return Ok(());
    };

    let Ok(entries) = fs::read_dir(&stamp_dir) else {
        return Ok(());
    };

    for entry in entries {
        let path = entry?.path();
        let Some(file_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
            continue;
        };
        let is_stamp =
            file_name.starts_with(&format!("{base_name}.")) && file_name.ends_with(".stamp");
        let is_rev = file_name == format!("{base_name}.rev");
        if is_stamp || is_rev {
            remove_file_if_present(&path, summary)?;
        }
    }
    Ok(())
}

fn unlink_state(roots: &Roots, name: &str, kind: Kind, summary: &mut Summary) -> Result<()> {
    let state_path = link_state::path(&roots.state_dir, name, kind);
    let had_state = state_path.exists();
    let links = link_state::read(&state_path)?;
    link_state::unlink_tracked(&state_path)?;

    for link in links {
        summary.note_removed(link);
    }
    if had_state {
        summary.note_removed(state_path);
    }
    Ok(())
}

fn manifest_path(path: &str, home: &Path) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        home.join(path)
    }
}

fn remove_any(path: &Path, summary: &mut Summary) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    summary.note_removed(path);
    Ok(())
}

fn remove_file_if_present(path: &Path, summary: &mut Summary) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => summary.note_removed(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_empty_install_parents(
    path: &Path,
    install_dir: &Path,
    summary: &mut Summary,
) -> Result<()> {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == install_dir || dir == Path::new("/") {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => summary.note_removed(dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // Parent cleanup is best-effort by design. These directories
                // commonly contain sibling deps (`github.com/owner/*`), and the
                // Bash implementation stops at the first `rmdir` failure. Keep
                // that safety bias here rather than turning a successfully
                // removed dependency into a hard failure because an ancestor was
                // non-empty or otherwise not removable.
                break;
            }
        }
        parent = dir.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::{method_transitions, remove_builtin, Roots};
    use crate::config::Entry;
    use crate::link_state::{self, Kind};
    use crate::manifest::{Manifest, ManifestEntry};

    #[test]
    fn method_transitions_ignore_orphans_and_return_method_changes() {
        let manifest = Manifest::parse(
            "repo|github:repo|repo|/tmp/repo\npkg|pkg|pkg|\norphan|cargo|orphan|/tmp/orphan\n",
        );
        let config = vec![entry("repo", "github:repo"), entry("pkg", "github:repo")];

        assert_eq!(
            method_transitions(&manifest, &config),
            vec![ManifestEntry::new("pkg", "pkg", "pkg", "")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_removes_symlink_install_and_tracked_links_but_preserves_target() {
        let fixture = Fixture::new("repo-symlink");
        let target = fixture.dir.join("target");
        let install_link = fixture.roots.home.join(".local/share/repo-tool");
        let short_bin = fixture.roots.bin_dir.join("repo-tool");
        let extra_bin = fixture.roots.bin_dir.join("repo-extra");
        let man_link = fixture.roots.home.join(".local/share/man/man1/repo-tool.1");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        fs::create_dir_all(short_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(man_link.parent().unwrap()).unwrap();
        symlink(&target, &install_link).unwrap();
        symlink(&target, &short_bin).unwrap();
        symlink(&target, &extra_bin).unwrap();
        symlink(&target, &man_link).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "repo-tool", Kind::Bin),
            std::slice::from_ref(&extra_bin),
        )
        .unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "repo-tool", Kind::Extras),
            std::slice::from_ref(&man_link),
        )
        .unwrap();
        fixture.write_state("repo-tool.repo.stamp", "1\n");

        remove_builtin(
            &ManifestEntry::new(
                "repo-tool",
                "github:repo",
                "repo-tool",
                ".local/share/repo-tool",
            ),
            &fixture.roots,
        )
        .unwrap();

        assert!(target.exists());
        assert!(fs::symlink_metadata(install_link).is_err());
        assert!(fs::symlink_metadata(short_bin).is_err());
        assert!(fs::symlink_metadata(extra_bin).is_err());
        assert!(fs::symlink_metadata(man_link).is_err());
        assert!(!fixture
            .roots
            .state_dir
            .join("repo-tool.repo.stamp")
            .exists());
    }

    #[test]
    fn binary_method_cleanup_removes_binary_install_root_empty_parents_and_stamps() {
        let fixture = Fixture::new("binary");
        let entry = ManifestEntry::new(
            "owner/binary-tool",
            "github:release",
            "binary-tool",
            fixture.roots.bin_dir.join("binary-tool").to_string_lossy(),
        );
        fixture.write_bin("binary-tool", "#!/bin/sh\n");
        fixture.write_install("owner/binary-tool/artifact", "data\n");
        fixture.write_state("owner/binary-tool.release.stamp", "1\n");

        remove_builtin(&entry, &fixture.roots).unwrap();

        assert!(!fixture.roots.bin_dir.join("binary-tool").exists());
        assert!(!fixture.roots.install_dir.join("owner/binary-tool").exists());
        assert!(!fixture.roots.install_dir.join("owner").exists());
        assert!(!fixture
            .roots
            .state_dir
            .join("owner/binary-tool.release.stamp")
            .exists());
    }

    #[test]
    fn package_cleanup_preserves_system_owned_artifacts() {
        let fixture = Fixture::new("pkg");
        fixture.write_bin("pkg-tool", "#!/bin/sh\n");

        let summary = remove_builtin(
            &ManifestEntry::new("pkg-tool", "pkg", "pkg-tool", ""),
            &fixture.roots,
        )
        .unwrap();

        assert!(summary.preserved_package);
        assert!(fixture.roots.bin_dir.join("pkg-tool").exists());
    }

    #[test]
    fn custom_cleanup_only_removes_shdeps_stamps() {
        let fixture = Fixture::new("custom");
        fixture.write_state("custom-tool.custom.stamp", "1\n");

        let summary = remove_builtin(
            &ManifestEntry::new("custom-tool", "custom", "custom-tool", ""),
            &fixture.roots,
        )
        .unwrap();

        assert!(summary.custom_requires_hook);
        assert!(!fixture
            .roots
            .state_dir
            .join("custom-tool.custom.stamp")
            .exists());
    }

    fn entry(name: &str, method: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            method: method.to_owned(),
            cmd: name.to_owned(),
            aliases: String::new(),
            filter: String::new(),
        }
    }

    struct Fixture {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shdeps-cleanup-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let roots = Roots {
                state_dir: dir.join("state"),
                install_dir: dir.join("share"),
                bin_dir: dir.join("bin"),
                home: dir.join("home"),
            };
            Self { dir, roots }
        }

        fn write_bin(&self, name: &str, content: &str) {
            self.write_at(&self.roots.bin_dir.join(name), content);
        }

        fn write_install(&self, rel: impl AsRef<Path>, content: &str) {
            self.write_at(&self.roots.install_dir.join(rel), content);
        }

        fn write_state(&self, rel: impl AsRef<Path>, content: &str) {
            self.write_at(&self.roots.state_dir.join(rel), content);
        }

        fn write_at(&self, path: &Path, content: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
}
