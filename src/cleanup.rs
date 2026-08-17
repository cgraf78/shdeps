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
/// can run arbitrary shell, so coordinators run them outside the non-reentrant
/// checkout lock and acquire that narrower lock only for deterministic filesystem
/// cleanup. The broader Shdeps state lock remains held across the full update or
/// prune transaction.
pub fn remove_builtin(entry: &ManifestEntry, roots: &Roots) -> Result<Summary> {
    if entry.method == method::GITHUB_REPO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo cleanup requires an acquired checkout-lock root",
        )
        .into());
    }
    remove_builtin_with_repo_root(entry, roots, None)
}

/// Removes built-in artifacts while honoring an already locked repo root.
///
/// Coordinators pass the normalized path yielded by checkout-lock acquisition
/// so a configured install-root symlink cannot be retargeted between locking
/// and cleanup. Non-repository methods ignore this argument.
pub(crate) fn remove_builtin_with_repo_root(
    entry: &ManifestEntry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<Summary> {
    if entry.method == method::GITHUB_REPO && locked_repo_root.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo cleanup requires an acquired checkout-lock root",
        )
        .into());
    }
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
            let repo_root = locked_repo_root.expect("validated above");
            unlink_state(roots, &entry.name, Kind::Bin, repo_root, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, repo_root, &mut summary)?;
            if let Some(path) = remove_legacy_repo_command(entry, roots, repo_root)? {
                summary.note_removed(path);
            }

            // Human-editable state may be missing or point at another managed
            // dependency. Cleanup authority comes only from the normalized
            // root yielded by the checkout lock.
            remove_any(repo_root, &mut summary)?;

            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        binary if method::is_binary_install_root(binary) => {
            let public_bin = roots.bin_dir.join(&entry.cmd);
            let install_root = roots.install_dir.join(&entry.name);
            let public_bin_is_symlink = fs::symlink_metadata(&public_bin)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            let public_bin_still_owned =
                !public_bin_is_symlink || points_into(&public_bin, &install_root);
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

            unlink_state(roots, &entry.name, Kind::Bin, &install_root, &mut summary)?;
            unlink_state(
                roots,
                &entry.name,
                Kind::Extras,
                &install_root,
                &mut summary,
            )?;
            if !preserve_public_launcher && public_bin_still_owned {
                remove_any(&public_bin, &mut summary)?;
            }

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

/// Removes only a legacy repo command symlink whose target proves Shdeps ownership.
///
/// Modern installs record every public command in `.binlinks`, so the normal
/// tracked unlink above is authoritative. This fallback exists for older state
/// that predated that ledger. A regular file is always unowned, and a symlink
/// is removed only when its literal target is the expected command under the
/// configured logical checkout or the checkout-lock-normalized physical root.
/// That rule preserves generated client adapters and foreign symlinks during
/// prune or method transitions without trusting human-editable manifest paths.
pub(crate) fn remove_legacy_repo_command(
    entry: &ManifestEntry,
    roots: &Roots,
    locked_repo_root: &Path,
) -> Result<Option<PathBuf>> {
    let canonical_name = config::canonical_name(&entry.name, method::GITHUB_REPO);
    let short = config::short_name(&canonical_name);
    let public = roots.bin_dir.join(short);
    let metadata = match fs::symlink_metadata(&public) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let target = fs::read_link(&public)?;
    let canonical_root = roots.install_dir.join(&canonical_name);
    let mut expected = vec![canonical_root.join("bin").join(short)];
    let physical = locked_repo_root.join("bin").join(short);
    if !expected.contains(&physical) {
        expected.push(physical);
    }
    if !expected.contains(&target) {
        return Ok(None);
    }
    fs::remove_file(&public)?;
    Ok(Some(public))
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

fn unlink_state(
    roots: &Roots,
    name: &str,
    kind: Kind,
    owner_root: &Path,
    summary: &mut Summary,
) -> Result<()> {
    let state_path = link_state::path(&roots.state_dir, name, kind);
    let had_state = state_path.exists();
    let removed =
        link_state::unlink_tracked_matching(&state_path, |link| points_into(link, owner_root))?;

    for link in removed {
        summary.note_removed(link);
    }
    if had_state {
        summary.note_removed(state_path);
    }
    Ok(())
}

fn points_into(path: &Path, root: &Path) -> bool {
    let resolved = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => match fs::read_link(path) {
            Ok(target) if target.is_absolute() => target,
            Ok(target) => path.parent().unwrap_or_else(|| Path::new("/")).join(target),
            Err(_) => return false,
        },
    };
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolved.starts_with(canonical_root)
}

/// Returns the only checkout root a manifest row may authorize for locking.
///
/// Repository ownership is structural: a valid dependency name owns exactly
/// `<install_dir>/<name>`. The human-editable `install_path` is diagnostic
/// state, not authority to select a sibling dependency or arbitrary path.
/// Physicalizing the configured install root also keeps cleanup on the same
/// root the checkout lock serialized when that root contains a symlink.
pub(crate) fn safe_repo_root(entry: &ManifestEntry, roots: &Roots) -> Option<PathBuf> {
    let name = config::canonical_name(&entry.name, method::GITHUB_REPO);
    if !config::valid_dep_name(&name) {
        return None;
    }
    Some(physical_install_root(&roots.install_dir).join(name))
}

/// Resolves the configured install root once before ownership-sensitive work.
pub(crate) fn physical_install_root(install_dir: &Path) -> PathBuf {
    fs::canonicalize(install_dir).unwrap_or_else(|_| install_dir.to_path_buf())
}

/// Recovers the physical install-root boundary from one locked repo path.
pub(crate) fn install_root_for_repo(repo_root: &Path, name: &str) -> Option<PathBuf> {
    let name = config::canonical_name(name, method::GITHUB_REPO);
    if !config::valid_dep_name(&name) {
        return None;
    }
    let mut root = repo_root;
    for _ in Path::new(&name).components() {
        root = root.parent()?;
    }
    Some(root.to_path_buf())
}

/// Validates manifest fields before they select hooks or artifact paths.
///
/// The manifest is intentionally human-editable and older versions did not
/// validate rows while loading them. Mutation coordinators therefore enforce
/// the current config grammar immediately before using a saved name or command
/// in any filesystem path, while leaving malformed state intact for recovery.
pub(crate) fn validate_manifest_artifact_entry(entry: &ManifestEntry) -> Result<()> {
    if !config::valid_dep_name(&entry.name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe dependency name in manifest: {}", entry.name),
        )
        .into());
    }
    if !config::valid_cmd_basename(&entry.cmd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe command name in manifest: {}", entry.cmd),
        )
        .into());
    }
    Ok(())
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
    use std::os::unix::fs::{MetadataExt, symlink};

    use super::{
        Roots, Summary, method_transitions, remove_builtin_with_repo_root, remove_stamps,
        safe_repo_root,
    };
    use crate::config::Entry;
    use crate::github_release_install;
    use crate::link_state::{self, Kind};
    use crate::manifest::{Manifest, ManifestEntry};

    fn remove_for_test(entry: &ManifestEntry, roots: &Roots) -> crate::Result<Summary> {
        let repo_root = (entry.method == crate::method::GITHUB_REPO)
            .then(|| safe_repo_root(entry, roots))
            .flatten();
        remove_builtin_with_repo_root(entry, roots, repo_root.as_deref())
    }

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
    fn github_repo_cleanup_rejects_missing_lock_authority() {
        let fixture = Fixture::new("repo-missing-lock-authority");
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");

        let error = super::remove_builtin(&entry, &fixture.roots).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires an acquired checkout-lock root")
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_removes_symlink_install_and_tracked_links_but_preserves_target() {
        let fixture = Fixture::new("repo-symlink");
        let target = fixture.dir.join("target");
        // The validated dependency name owns this exact install root. The
        // recorded path is retained only to prove that it cannot redirect
        // cleanup elsewhere.
        let install_link = fixture.roots.install_dir.join("repo-tool");
        let short_bin = fixture.roots.bin_dir.join("repo-tool");
        let extra_bin = fixture.roots.bin_dir.join("repo-extra");
        // Extras live wherever the linker placed them. `man_link` is
        // just a tracked symlink used to verify unlink_tracked clears
        // it; the path does not need to be in install_dir.
        let man_link = fixture.dir.join("home/.local/share/man/man1/repo-tool.1");
        fs::create_dir_all(target.join("bin")).unwrap();
        fs::write(target.join("bin/repo-tool"), "#!/bin/sh\n").unwrap();
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        fs::create_dir_all(short_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(man_link.parent().unwrap()).unwrap();
        symlink(&target, &install_link).unwrap();
        symlink(install_link.join("bin/repo-tool"), &short_bin).unwrap();
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

        remove_for_test(
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
    fn github_repo_cleanup_preserves_unowned_regular_command() {
        let fixture = Fixture::new("repo-regular-command");
        let install_root = fixture.roots.install_dir.join("owner/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(install_root.join("bin")).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        fs::write(install_root.join("bin/tool"), "managed command").unwrap();
        fs::write(&public, "generated client adapter").unwrap();
        let before = fs::metadata(&public).unwrap();

        let summary = remove_for_test(
            &ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            &fixture.roots,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "generated client adapter"
        );
        let after = fs::metadata(&public).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        assert!(!install_root.exists());
        assert!(!summary.removed.contains(&public));
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin).exists(),
            "preserved regular commands must never acquire Shdeps ownership state"
        );
    }

    #[test]
    fn github_repo_cleanup_cannot_target_another_managed_dependency() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-cross-dependency-path");
        let owned = fixture.roots.install_dir.join("owner/a");
        let other = fixture.roots.install_dir.join("owner/b");
        let public = fixture.roots.bin_dir.join("a");
        fs::create_dir_all(&owned).unwrap();
        fs::create_dir_all(other.join("bin")).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        fs::write(owned.join("owned"), "remove\n").unwrap();
        fs::write(other.join("sentinel"), "preserve\n").unwrap();
        fs::write(other.join("bin/a"), "other command\n").unwrap();
        symlink(other.join("bin/a"), &public).unwrap();

        remove_for_test(
            &ManifestEntry::new("owner/a", "github:repo", "a", other.display().to_string()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!owned.exists());
        assert_eq!(
            fs::read_to_string(other.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(
            fs::symlink_metadata(public)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_uses_locked_root_after_install_alias_retarget() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-locked-root-retarget");
        let physical_a = fixture.dir.join("physical-a");
        let physical_b = fixture.dir.join("physical-b");
        let logical = fixture.dir.join("install-link");
        let root_a = physical_a.join("owner/tool");
        let root_b = physical_b.join("owner/tool");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_a.join("owned"), "remove\n").unwrap();
        fs::write(root_b.join("sentinel"), "preserve\n").unwrap();
        symlink(&physical_a, &logical).unwrap();
        let roots = Roots {
            install_dir: logical.clone(),
            ..fixture.roots.clone()
        };
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");
        let locked_root = safe_repo_root(&entry, &roots).unwrap();
        assert_eq!(locked_root, root_a);

        fs::remove_file(&logical).unwrap();
        symlink(&physical_b, &logical).unwrap();
        remove_builtin_with_repo_root(&entry, &roots, Some(&locked_root)).unwrap();

        assert!(!root_a.exists());
        assert_eq!(
            fs::read_to_string(root_b.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(fs::read_link(logical).unwrap(), physical_b);
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

        remove_for_test(&entry, &fixture.roots).unwrap();

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
    fn binary_method_cleanup_preserves_tracked_command_retargeted_to_another_install() {
        let fixture = Fixture::new("binary-retargeted-command");
        let old_root = fixture.roots.install_dir.join("old-tool");
        let replacement_root = fixture.roots.install_dir.join("replacement-tool");
        let old_target = old_root.join("bin/tool");
        let replacement_target = replacement_root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");

        fs::create_dir_all(old_target.parent().unwrap()).unwrap();
        fs::write(&old_target, "old command\n").unwrap();
        fs::create_dir_all(replacement_target.parent().unwrap()).unwrap();
        fs::write(&replacement_target, "replacement command\n").unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&replacement_target, &public).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "old-tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();

        let summary = remove_for_test(
            &ManifestEntry::new("old-tool", "cargo", "tool", old_root.to_string_lossy()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!old_root.exists());
        assert_eq!(
            fs::read_link(&public).unwrap(),
            replacement_target,
            "stale path-only ownership must not remove another install's live command"
        );
        assert!(replacement_root.exists());
        assert!(!summary.removed.contains(&public));
        assert!(!link_state::path(&fixture.roots.state_dir, "old-tool", Kind::Bin).exists());
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

        remove_for_test(&entry, &fixture.roots).unwrap();

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

        let summary = remove_for_test(
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

        let summary = remove_for_test(
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
    fn safe_repo_root_uses_named_fallback_when_recorded_path_is_empty() {
        let dir = crate::test_support::temp_dir("shdeps-cleanup-empty-repo-path");
        let roots = Roots {
            state_dir: dir.join("state"),
            install_dir: dir.join("home"),
            bin_dir: dir.join("bin"),
        };
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");

        assert_eq!(
            safe_repo_root(&entry, &roots),
            Some(roots.install_dir.join("owner/tool"))
        );
    }

    #[test]
    fn safe_repo_root_canonicalizes_legacy_git_suffix() {
        let fixture = Fixture::new("repo-root-git-suffix");
        let entry = ManifestEntry::new("owner/tool.git", "github:repo", "tool", "");

        assert_eq!(
            safe_repo_root(&entry, &fixture.roots),
            Some(fixture.roots.install_dir.join("owner/tool"))
        );
    }

    #[test]
    fn github_repo_cleanup_ignores_tampered_recorded_install_path() {
        // Repository cleanup derives ownership from the validated name. A
        // recorded absolute path is diagnostic only and cannot redirect it.
        let fixture = Fixture::new("tampered-install-path");
        let bystander = fixture.dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("data"), "user-owned").unwrap();
        let absolute = bystander.to_string_lossy().into_owned();

        remove_for_test(
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
