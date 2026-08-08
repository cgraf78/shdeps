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

use crate::Result;
use crate::config;
use crate::github_release_install::{self, ArchiveState};
use crate::link_state::{self, Kind};
use crate::manifest::{Manifest, ManifestEntry};
use crate::method;

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
        method::PKG => {
            // System packages are explicitly not owned by shdeps. Migrating a
            // dependency from `pkg` to another method must clear shdeps tracking
            // later, but uninstalling the OS package would be surprising and
            // potentially destructive.
            summary.preserved_package = true;
        }
        method::GITHUB_REPO => {
            unlink_state(roots, &entry.name, Kind::Bin, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, &mut summary)?;

            if !entry.install_path.is_empty() {
                // The manifest file is human-editable text. A corrupted or
                // hand-edited `install_path` value (absolute path or
                // `..`-escape into another tree) would otherwise hand
                // `remove_dir_all` an arbitrary path. `safe_managed_path`
                // refuses anything not lexically contained under
                // `install_dir` or `home`, so the worst case is a skipped
                // cleanup with a noted leftover rather than a destructive
                // delete outside the shdeps-managed tree.
                if let Some(install_path) = safe_managed_path(&entry.install_path, roots) {
                    remove_any(&install_path, &mut summary)?;
                }
            }

            remove_any(
                &roots.bin_dir.join(config::short_name(&entry.name)),
                &mut summary,
            )?;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        binary if method::is_binary_install_root(binary) => {
            let public_bin = roots.bin_dir.join(&entry.cmd);
            let preserve_public_launcher = if binary == method::GITHUB_RELEASE {
                github_release_install::archive_state(
                    &roots.state_dir,
                    &roots.install_dir,
                    &public_bin,
                    &entry.name,
                )? != ArchiveState::None
                    && github_release_install::is_non_symlink(&public_bin)
            } else {
                false
            };

            unlink_state(roots, &entry.name, Kind::Bin, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, &mut summary)?;
            if !preserve_public_launcher {
                remove_any(&public_bin, &mut summary)?;
            }

            let install_root = roots.install_dir.join(&entry.name);
            remove_any(&install_root, &mut summary)?;
            remove_empty_install_parents(&install_root, &roots.install_dir, &mut summary)?;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        method::CUSTOM => {
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
        // All known stamp kinds are single words without dots (repo, release,
        // github, cargo, etc.). Requiring a dot-free middle prevents
        // accidentally matching stamps for a dep whose name starts with the
        // same prefix, e.g. `tool.extra.repo.stamp` when base_name is `tool`.
        let is_stamp = file_name
            .strip_prefix(&format!("{base_name}."))
            .and_then(|s| s.strip_suffix(".stamp"))
            .is_some_and(|kind| !kind.contains('.'));
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

/// Returns a manifest-supplied install path only when it lexically
/// resolves inside the shdeps-managed install tree.
///
/// The manifest is a human-readable text file with no schema
/// enforcement, so a corrupt record or a hand edit could plant any
/// absolute path or a `..`-escape into `entry.install_path`. Without
/// this guard, the cleanup path would hand that value straight to
/// `remove_dir_all`. Rather than canonicalize (which would follow
/// symlinks and could mask escapes through link targets), the check
/// rejects anything with a `..` component and requires lexical
/// containment under `install_dir`.
///
/// Earlier versions of this predicate also accepted any path under
/// `$HOME`. That was overly permissive: a tampered manifest entry
/// pointing at, say, `$HOME/.ssh` or `$HOME/Documents/project` would
/// pass the guard and be removed during prune. shdeps's own writes
/// always target `install_dir` (configurable via `SHDEPS_INSTALL_DIR`,
/// defaulting to `$HOME/.local/share`), so the install-tree
/// containment is sufficient for legitimate entries. Relative paths
/// written by older fleet bootstraps (e.g., `.local/share/<dep>`)
/// still work in the default configuration because they resolve to
/// `$HOME/.local/share/<dep>` which IS under the default
/// `install_dir`.
///
/// Visible to `update_transition::cleanup_snapshot`, which has its
/// own `github:repo` cleanup path and must apply the same
/// containment hardening. Without sharing this predicate, the
/// second consumer silently bypasses the install-path guard.
pub(crate) fn safe_managed_path(install_path: &str, roots: &Roots) -> Option<PathBuf> {
    let path = manifest_path(install_path, &roots.home);
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    path.starts_with(&roots.install_dir).then_some(path)
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

    use super::{
        Roots, Summary, method_transitions, remove_builtin, remove_stamps, safe_managed_path,
    };
    use crate::config::Entry;
    use crate::github_release_install;
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
        // Install path is under `install_dir` (the only tree
        // `safe_managed_path` accepts since the round-6 tightening
        // of the `$HOME` acceptance).
        let install_link = fixture.roots.install_dir.join("repo-tool");
        let short_bin = fixture.roots.bin_dir.join("repo-tool");
        let extra_bin = fixture.roots.bin_dir.join("repo-extra");
        // Extras live wherever the linker placed them. `man_link` is
        // just a tracked symlink used to verify unlink_tracked clears
        // it; the path does not need to be in install_dir.
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
                install_link.to_string_lossy(),
            ),
            &fixture.roots,
        )
        .unwrap();

        assert!(target.exists());
        assert!(fs::symlink_metadata(install_link).is_err());
        assert!(fs::symlink_metadata(short_bin).is_err());
        assert!(fs::symlink_metadata(extra_bin).is_err());
        assert!(fs::symlink_metadata(man_link).is_err());
        assert!(
            !fixture
                .roots
                .state_dir
                .join("repo-tool.repo.stamp")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn binary_method_cleanup_removes_binary_install_root_empty_parents_and_stamps() {
        let fixture = Fixture::new("binary");
        let entry = ManifestEntry::new(
            "owner/binary-tool",
            "github:release",
            "binary-tool",
            fixture.roots.bin_dir.join("binary-tool").to_string_lossy(),
        );
        let helper = fixture.roots.bin_dir.join("binary-helper");
        let helper_target = fixture
            .roots
            .install_dir
            .join("owner/binary-tool/bin/binary-helper");
        fs::create_dir_all(helper_target.parent().unwrap()).unwrap();
        fs::write(&helper_target, "#!/bin/sh\n").unwrap();
        let tool_target = fixture
            .roots
            .install_dir
            .join("owner/binary-tool/bin/binary-tool");
        fs::write(&tool_target, "#!/bin/sh\n").unwrap();
        fs::create_dir_all(fixture.roots.bin_dir.clone()).unwrap();
        symlink(&tool_target, fixture.roots.bin_dir.join("binary-tool")).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        symlink(&helper_target, &helper).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/binary-tool", Kind::Bin),
            &[fixture.roots.bin_dir.join("binary-tool"), helper.clone()],
        )
        .unwrap();
        fixture.write_install("owner/binary-tool/artifact", "data\n");
        fixture.write_state("owner/binary-tool.release.stamp", "1\n");

        remove_builtin(&entry, &fixture.roots).unwrap();

        assert!(!fixture.roots.bin_dir.join("binary-tool").exists());
        assert!(!helper.exists());
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/binary-tool", Kind::Bin).exists()
        );
        assert!(!fixture.roots.install_dir.join("owner/binary-tool").exists());
        assert!(!fixture.roots.install_dir.join("owner").exists());
        assert!(
            !fixture
                .roots
                .state_dir
                .join("owner/binary-tool.release.stamp")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_cleanup_preserves_regular_public_launcher() {
        let fixture = Fixture::new("archive-launcher");
        let entry = ManifestEntry::new(
            "owner/archive-tool",
            "github:release",
            "archive-tool",
            fixture.roots.bin_dir.join("archive-tool").to_string_lossy(),
        );
        let public = fixture.roots.bin_dir.join("archive-tool");
        let helper = fixture.roots.bin_dir.join("archive-helper");
        let helper_target = fixture
            .roots
            .install_dir
            .join("owner/archive-tool/bin/archive-helper");
        fs::create_dir_all(helper_target.parent().unwrap()).unwrap();
        fs::write(&helper_target, "#!/bin/sh\n").unwrap();
        fixture.write_bin("archive-tool", "#!/bin/sh\n# user launcher\n");
        symlink(&helper_target, &helper).unwrap();
        fs::write(
            github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/archive-tool",
            ),
            "v1 archive\n",
        )
        .unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/archive-tool", Kind::Bin),
            std::slice::from_ref(&helper),
        )
        .unwrap();

        remove_builtin(&entry, &fixture.roots).unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "#!/bin/sh\n# user launcher\n"
        );
        assert!(!helper.exists());
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/archive-tool", Kind::Bin).exists()
        );
        assert!(
            !fixture
                .roots
                .install_dir
                .join("owner/archive-tool")
                .exists()
        );
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
        assert!(
            !fixture
                .roots
                .state_dir
                .join("custom-tool.custom.stamp")
                .exists()
        );
    }

    #[test]
    fn remove_stamps_does_not_delete_stamps_for_dep_with_matching_name_prefix() {
        // Two deps in the same namespace directory — `owner/tool` and
        // `owner/tool.extra` — share the `tool.` filename prefix. Pruning one
        // must not delete the other's stamps.
        let fixture = Fixture::new("stamp-prefix");
        fixture.write_state("owner/tool.repo.stamp", "1\n");
        fixture.write_state("owner/tool.extra.repo.stamp", "1\n");

        let mut summary = Summary::default();
        remove_stamps(&fixture.roots.state_dir, "owner/tool", &mut summary).unwrap();

        assert!(
            !fixture
                .roots
                .state_dir
                .join("owner/tool.repo.stamp")
                .exists(),
            "tool's own stamp should be removed"
        );
        assert!(
            fixture
                .roots
                .state_dir
                .join("owner/tool.extra.repo.stamp")
                .exists(),
            "tool.extra's stamp must not be removed by pruning tool"
        );
    }

    #[test]
    fn safe_managed_path_accepts_relative_paths_that_resolve_under_install_dir() {
        // The historical Bash layout stored install paths as
        // `.local/share/...` relative to `$HOME`. In a real shdeps
        // configuration `install_dir` defaults to
        // `$HOME/.local/share`, so the relative path resolves UNDER
        // `install_dir` and remains acceptable. The fixture below
        // mirrors that real-world overlap so the legacy entry passes.
        let dir = crate::test_support::temp_dir("shdeps-cleanup-safe-rel");
        let roots = Roots {
            state_dir: dir.join("state"),
            install_dir: dir.join("home/.local/share"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let resolved = safe_managed_path(".local/share/repo-tool", &roots).unwrap();
        assert_eq!(resolved, roots.home.join(".local/share/repo-tool"));
        // Sanity-check that the resolved path is indeed under
        // install_dir (the load-bearing containment).
        assert!(resolved.starts_with(&roots.install_dir));
    }

    #[test]
    fn safe_managed_path_rejects_paths_under_home_but_outside_install_dir() {
        // Round-6 codex finding: a tampered manifest record pointing
        // at e.g. `$HOME/.ssh` or `$HOME/Documents/private-project`
        // used to pass the guard because the predicate accepted any
        // path under `$HOME`. The new predicate restricts to
        // `install_dir`, which keeps sensitive home subdirs out of
        // reach of the prune cleanup loop.
        let fixture = Fixture::new("safe-home-but-not-install");
        // The Fixture's `install_dir` is `dir/share`; `home` is
        // `dir/home`. Both `.ssh/id_rsa` and a sibling under home
        // resolve outside install_dir.
        assert!(safe_managed_path(".ssh/id_rsa", &fixture.roots).is_none());
        assert!(safe_managed_path("Documents/project", &fixture.roots).is_none());
        let abs_home_sensitive = fixture
            .roots
            .home
            .join(".gnupg")
            .to_string_lossy()
            .into_owned();
        assert!(safe_managed_path(&abs_home_sensitive, &fixture.roots).is_none());
    }

    #[test]
    fn safe_managed_path_accepts_absolute_paths_under_install_dir() {
        // Newer github:repo records may carry an absolute path under the
        // managed install root. Lexical containment of `install_dir` covers
        // that case.
        let fixture = Fixture::new("safe-abs-install");
        let absolute = fixture.roots.install_dir.join("owner/repo");
        let resolved = safe_managed_path(absolute.to_str().unwrap(), &fixture.roots).unwrap();
        assert_eq!(resolved, absolute);
    }

    #[test]
    fn safe_managed_path_rejects_absolute_path_outside_managed_roots() {
        // A corrupt manifest record could otherwise hand `remove_dir_all` an
        // arbitrary system path. The guard must refuse it rather than treat
        // the record as authoritative.
        let fixture = Fixture::new("safe-escape-abs");
        assert!(safe_managed_path("/etc/passwd", &fixture.roots).is_none());
        assert!(safe_managed_path("/tmp/unrelated", &fixture.roots).is_none());
    }

    #[test]
    fn safe_managed_path_rejects_parent_dir_escapes() {
        // `..` segments would lexically allow a path to escape the
        // `starts_with` check even when the prefix matches one of the
        // managed roots. They are refused outright.
        let fixture = Fixture::new("safe-escape-parent");
        assert!(safe_managed_path("../../etc/passwd", &fixture.roots).is_none());
        assert!(safe_managed_path(".local/share/../../../etc", &fixture.roots).is_none());
    }

    #[test]
    fn safe_managed_path_accepts_curdir_components_in_legitimate_paths() {
        // An older fleet bootstrap could have written paths like
        // `./<dep>` (with a leading CurDir) into the manifest. `.`
        // components are benign on their own (`join` resolves them
        // away), so rejecting them along with `..` would silently
        // skip cleanup for legitimate older entries. Only `..` is
        // the real escape vector. The fixture below uses an
        // install_dir-rooted relative path so the post-round-6
        // containment guard accepts it.
        let dir = crate::test_support::temp_dir("shdeps-cleanup-safe-curdir");
        let roots = Roots {
            state_dir: dir.join("state"),
            install_dir: dir.join("home/.local/share"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let resolved = safe_managed_path("./.local/share/repo-tool", &roots).unwrap();
        assert!(resolved.starts_with(&roots.install_dir));
    }

    #[test]
    fn github_repo_cleanup_skips_install_path_outside_managed_roots() {
        // End-to-end: a tampered manifest record with an absolute escape path
        // must not result in `remove_dir_all` being called on that path. The
        // bin-dir cleanup and stamp removal still run, but the external file
        // must be left untouched.
        let fixture = Fixture::new("tampered-install-path");
        let bystander = fixture.dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("data"), "user-owned").unwrap();
        let absolute = bystander.to_string_lossy().into_owned();

        remove_builtin(
            &ManifestEntry::new("repo-tool", "github:repo", "repo-tool", &absolute),
            &fixture.roots,
        )
        .unwrap();

        assert!(bystander.exists(), "external path must be preserved");
        assert!(bystander.join("data").exists());
    }

    fn entry(name: &str, method: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            method: method.to_owned(),
            cmd: name.to_owned(),
            cmd_explicit: false,
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
            let dir = crate::test_support::temp_dir(&format!("shdeps-cleanup-{name}"));
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
