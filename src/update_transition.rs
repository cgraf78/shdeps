//! Method-transition helpers for `shdeps update`.
//!
//! Update methods write new artifacts into the same public paths and state
//! files that old methods used. This module snapshots old ownership before the
//! install starts, then cleans only that snapshot after the new method succeeds.
//! Keeping the transaction-sensitive code here prevents each installer from
//! learning a partial version of the same migration rules.

use std::collections::{BTreeSet, HashMap};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::cleanup;
use crate::config::{self, Entry};
use crate::github_release_install::{self, ArchiveState};
use crate::link_state::{self, Kind};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::method;
use crate::runtime::Roots;
use crate::update::Item;

/// Pre-install snapshot for a configured method transition.
#[derive(Debug, Clone)]
pub(crate) struct Transition {
    old: ManifestEntry,
    bin_links: Vec<PathBuf>,
    extra_links: Vec<PathBuf>,
    archive_state: ArchiveState,
    archive_root_identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// Builds a transition map keyed by dependency name.
pub(crate) fn by_name(
    manifest: &Manifest,
    entries: &[Entry],
    roots: &Roots,
) -> Result<HashMap<String, Transition>> {
    cleanup::method_transitions(manifest, entries)
        .into_iter()
        .map(|entry| {
            // A saved row is human-editable state. Validate it before its name
            // or command participates in link-state and public-bin paths.
            cleanup::validate_manifest_artifact_entry(&entry)?;
            let bin_links =
                link_state::read(&link_state::path(&roots.state_dir, &entry.name, Kind::Bin))?;
            let extra_links = link_state::read(&link_state::path(
                &roots.state_dir,
                &entry.name,
                Kind::Extras,
            ))?;
            let archive_state = if entry.method == method::GITHUB_RELEASE {
                github_release_install::archive_state(
                    &roots.state_dir,
                    &roots.install_dir,
                    &roots.bin_dir.join(&entry.cmd),
                    &entry.name,
                )?
            } else {
                ArchiveState::None
            };
            let archive_root_identity = if entry.method == method::GITHUB_RELEASE {
                file_identity(&roots.install_dir.join(&entry.name))?
            } else {
                None
            };
            Ok((
                entry.name.clone(),
                Transition {
                    old: entry,
                    bin_links,
                    extra_links,
                    archive_state,
                    archive_root_identity,
                },
            ))
        })
        .collect()
}

/// Returns the old manifest row for a transition.
pub(crate) fn old(transition: &Transition) -> &ManifestEntry {
    &transition.old
}

/// Returns whether the previous built-in method proves ownership of the
/// canonical install root that a new `github:repo` install will reuse.
///
/// External toolchains always install beneath that managed root. A GitHub
/// release owns it only when archive evidence is proven; raw release binaries,
/// packages, and custom hooks do not authorize deleting a coincidental path.
pub(crate) fn owns_repo_destination(transition: &Transition) -> bool {
    method::is_external(&transition.old.method)
        || (transition.old.method == method::GITHUB_RELEASE
            && transition.archive_state == ArchiveState::Proven)
}

/// Refreshes filesystem-derived transition evidence under the checkout lock.
///
/// `by_name` runs before workers acquire dependency checkout locks. Structural
/// state in the manifest remains valid across that wait, but an installer may
/// legally replace a release root while holding the shared lock. Re-reading the
/// archive marker and live links from the lock-normalized physical root keeps a
/// stale pre-worker snapshot from authorizing deletion of the new generation.
pub(crate) fn revalidate_for_repo_install(
    transition: Option<&Transition>,
    roots: &Roots,
    locked_repo_root: &Path,
) -> Result<Option<Transition>> {
    let Some(transition) = transition else {
        return Ok(None);
    };
    let mut refreshed = transition.clone();
    if refreshed.old.method == method::GITHUB_RELEASE {
        let install_base = cleanup::install_root_for_repo(locked_repo_root, &refreshed.old.name)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cannot derive install root from acquired checkout-lock path",
                )
            })?;
        let explicit =
            github_release_install::explicit_archive_state(&install_base, &refreshed.old.name)?;
        let current_identity = file_identity(locked_repo_root)?;
        let checkout_metadata_present = path_entry_exists(&locked_repo_root.join(".git"))?;
        refreshed.archive_state = if explicit == ArchiveState::Proven {
            ArchiveState::Proven
        } else if !checkout_metadata_present
            && refreshed.archive_state == ArchiveState::Proven
            && same_file_identity(refreshed.archive_root_identity, current_identity)
        {
            github_release_install::archive_state(
                &roots.state_dir,
                &install_base,
                &roots.bin_dir.join(&refreshed.old.cmd),
                &refreshed.old.name,
            )?
        } else {
            // Legacy archive proof is path-based: a public symlink can keep
            // resolving after another writer replaces the directory at the
            // same name. It authorizes only the exact root generation whose
            // identity was observed before waiting for the checkout lock.
            ArchiveState::None
        };
        refreshed.archive_root_identity = current_identity;
    }
    Ok(Some(refreshed))
}

// Snapshot one real directory generation on Unix; platforms without a stable
// inode API intentionally cannot retain legacy path-only archive proof.
fn file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Ok(None);
    }

    #[cfg(unix)]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

// Require a present, unchanged Unix identity rather than allowing None == None
// to become accidental ownership evidence.
fn same_file_identity(before: Option<FileIdentity>, current: Option<FileIdentity>) -> bool {
    #[cfg(unix)]
    {
        before.is_some() && before == current
    }
    #[cfg(not(unix))]
    {
        let _ = (before, current);
        false
    }
}

// Probe a no-follow ownership marker while preserving all errors except true
// absence; a malformed .git entry must still disqualify legacy archive proof.
fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Runs an installer with transition-only public-bin preparation.
pub(crate) fn install_with_prepared(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    install: impl FnOnce() -> Result<Item>,
) -> Result<Item> {
    let backup = prepare_public_bin(entry, transition, roots)?;
    let result = install();

    match &result {
        Ok(item) if item.failed => restore_public_bin_backup(backup)?,
        Ok(_) => discard_public_bin_backup(backup)?,
        Err(_) => restore_public_bin_backup(backup)?,
    }

    result
}

/// Cleans old artifacts after a new method has successfully recorded itself.
pub(crate) fn cleanup_successful(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<bool> {
    let Some(transition) = transition else {
        return Ok(false);
    };
    if transition.old.method == method::GITHUB_REPO && locked_repo_root.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo transition cleanup requires an acquired checkout-lock root",
        )
        .into());
    }

    Ok(cleanup_snapshot(entry, transition, roots, locked_repo_root).is_err())
}

/// Restores the old manifest row after a transition install fails.
pub(crate) fn restore_failed(transition: Option<&Transition>, manifest_path: &Path) -> Result<()> {
    if let Some(transition) = transition {
        manifest::upsert(manifest_path, transition.old.clone())?;
    }
    Ok(())
}

#[derive(Debug)]
struct PublicBinBackup {
    original: PathBuf,
    backup: PathBuf,
}

fn prepare_public_bin(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
) -> Result<Option<PublicBinBackup>> {
    let Some(transition) = transition else {
        return Ok(None);
    };

    if transition.old.method != method::GITHUB_RELEASE
        || transition.old.cmd != entry.cmd
        || !method::is_symlink_install_root(&entry.method)
    {
        return Ok(None);
    }

    let original = roots.bin_dir.join(&entry.cmd);
    match transition.archive_state {
        ArchiveState::Proven if github_release_install::is_non_symlink(&original) => {
            // Archive installs never own a regular launcher they preserved.
            // Moving it aside here would let the new method replace it and the
            // success path would then discard the only copy.
            return Ok(None);
        }
        ArchiveState::Ambiguous => {
            return Err(std::io::Error::other(format!(
                "refusing to replace ambiguous legacy release command: {}",
                original.display()
            ))
            .into());
        }
        ArchiveState::None | ArchiveState::Proven => {}
    }
    let metadata = match fs::symlink_metadata(&original) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(None);
    }

    // Raw `github:release` installs are the one built-in method that owns a
    // regular file directly in SHDEPS_BIN_DIR. Symlink-based methods correctly
    // preserve regular files in the general case, but during this specific
    // method transition that preservation would keep the old release binary in
    // front of the newly installed tool. Move the owned file aside just for the
    // install attempt, then either discard it after success or restore it after
    // failure so a bad migration does not leave the command missing.
    let backup = backup_path(&original);
    remove_any(&backup)?;
    fs::rename(&original, &backup)?;
    Ok(Some(PublicBinBackup { original, backup }))
}

fn restore_public_bin_backup(backup: Option<PublicBinBackup>) -> Result<()> {
    let Some(backup) = backup else {
        return Ok(());
    };
    remove_any(&backup.original)?;
    fs::rename(&backup.backup, &backup.original)?;
    Ok(())
}

fn discard_public_bin_backup(backup: Option<PublicBinBackup>) -> Result<()> {
    if let Some(backup) = backup {
        remove_any(&backup.backup)?;
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.to_path_buf();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("command");
    backup.set_file_name(format!(".{name}.transition.{}", std::process::id()));
    backup
}

fn cleanup_snapshot(
    entry: &Entry,
    transition: &Transition,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<()> {
    let preserve = preserve_paths(entry, roots, locked_repo_root)?;

    match transition.old.method.as_str() {
        method::PKG => {
            // System packages are not shdeps-owned. Once another method has
            // succeeded, the manifest swap is enough; the OS package remains
            // available for anything else on the machine that might use it.
        }
        method::GITHUB_REPO => {
            let install_path = locked_repo_root.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "github:repo transition cleanup requires an acquired checkout-lock root",
                )
            })?;
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Bin,
                &transition.bin_links,
                &preserve,
            )?;
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Extras,
                &transition.extra_links,
                &preserve,
            )?;

            // Repository ownership comes from the validated dependency name,
            // never the human-editable recorded install path.
            if !preserve.contains(install_path) {
                remove_any(install_path)?;
                let install_root =
                    crate::cleanup::install_root_for_repo(install_path, &transition.old.name)
                        .unwrap_or_else(|| {
                            crate::cleanup::physical_install_root(&roots.install_dir)
                        });
                remove_empty_install_parents(install_path, &install_root)?;
            }

            let legacy_bin = roots.bin_dir.join(config::short_name(&transition.old.name));
            if !preserve.contains(&legacy_bin) {
                let cleanup_roots = crate::cleanup::Roots {
                    state_dir: roots.state_dir.clone(),
                    install_dir: roots.install_dir.clone(),
                    bin_dir: roots.bin_dir.clone(),
                };
                crate::cleanup::remove_legacy_repo_command(
                    &transition.old,
                    &cleanup_roots,
                    install_path,
                )?;
            }
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        binary if method::is_binary_install_root(binary) => {
            let public_bin = roots.bin_dir.join(&transition.old.cmd);
            let preserve_public_launcher = binary == method::GITHUB_RELEASE
                && transition.archive_state != ArchiveState::None
                && github_release_install::is_non_symlink(&public_bin);
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Bin,
                &transition.bin_links,
                &preserve,
            )?;
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Extras,
                &transition.extra_links,
                &preserve,
            )?;
            if !preserve_public_launcher {
                remove_any_unless_preserved(&public_bin, &preserve)?;
            }

            let install_root = roots.install_dir.join(&transition.old.name);
            if !preserve.contains(&install_root) {
                remove_any(&install_root)?;
                remove_empty_install_parents(&install_root, &roots.install_dir)?;
            }
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        method::CUSTOM => remove_stamps(&roots.state_dir, &transition.old.name)?,
        _ => {}
    }

    Ok(())
}

fn preserve_paths(
    entry: &Entry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<BTreeSet<PathBuf>> {
    let mut preserve = BTreeSet::new();

    // New-method link state may live at the same `<name>.links` path as the
    // old method. Read it after the install succeeds so transition cleanup can
    // remove only pre-existing links without deleting the freshly linked public
    // command, man page, or completion.
    if matches!(
        entry.method.as_str(),
        method::GITHUB_REPO | method::GITHUB_RELEASE
    ) {
        for kind in [Kind::Bin, Kind::Extras] {
            if let Ok(links) =
                link_state::read(&link_state::path(&roots.state_dir, &entry.name, kind))
            {
                preserve.extend(links);
            }
        }
    }

    match entry.method.as_str() {
        symlink if method::is_symlink_install_root(symlink) => {
            preserve.insert(roots.bin_dir.join(&entry.cmd));
            preserve.insert(locked_repo_root.map_or_else(
                || crate::cleanup::physical_install_root(&roots.install_dir).join(&entry.name),
                Path::to_path_buf,
            ));
        }
        method::GITHUB_RELEASE => {
            let public_bin = roots.bin_dir.join(&entry.cmd);
            preserve.insert(public_bin.clone());

            // A repo-to-release transition holds the checkout lock for one
            // physical root. Treat that path as the ownership authority all
            // the way through cleanup: the configured install directory may be
            // a symlink that is retargeted after the release installer writes
            // the new archive but before this preservation check runs.
            let install_base = match locked_repo_root {
                Some(repo_root) => crate::cleanup::install_root_for_repo(repo_root, &entry.name)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cannot derive install root from acquired checkout-lock path",
                        )
                    })?,
                None => crate::cleanup::physical_install_root(&roots.install_dir),
            };
            let install_root = locked_repo_root
                .map(Path::to_path_buf)
                .unwrap_or_else(|| install_base.join(&entry.name));
            if github_release_install::archive_state(
                &roots.state_dir,
                &install_base,
                &public_bin,
                &entry.name,
            )? == ArchiveState::Proven
                || release_install_root_is_owned(&install_root, &public_bin)
            {
                preserve.insert(install_root);
            }
        }
        _ => {}
    }

    Ok(preserve)
}

fn release_install_root_is_owned(path: &Path, public_bin: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };

    // `github:release` has two ownership modes. Raw/compressed single-file
    // assets own only the public binary, while archive assets replace the
    // managed install root with a real directory and then symlink the public
    // command into it. During a method transition from `github:repo`, that same
    // path may still be a symlink to a local development checkout. Preserving
    // the symlink would leave stale repo state behind and make `dep-root` point
    // at a clone even though the configured method is now release-based. A real
    // directory is only safe to keep when the public command actually resolves
    // into it. A repo -> raw-release transition can leave an old real checkout
    // directory at the same path; preserving it would make the manifest say
    // release while `dep-root` and cleanup still see stale repo-owned files.
    // The public command is the legacy proof of archive ownership because raw
    // release installs write a regular bin file, while old archive installs
    // exposed the selected binary from inside the extracted root. New archives
    // carry an explicit marker handled by `archive_state` above.
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }

    points_into(public_bin, path)
}

fn points_into(path: &Path, root: &Path) -> bool {
    // Fully resolve all symlink levels so chained symlinks (e.g. bin/tool ->
    // share/name/current -> share/name/v1.2.3/bin/tool) are handled correctly.
    // canonicalize fails for broken symlinks or targets that don't exist yet
    // during concurrent installs, so fall back to single-level read_link in
    // that case — the one-level result is still useful for the primary check.
    let resolved = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            if !fs::symlink_metadata(path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                path.to_path_buf()
            } else {
                match fs::read_link(path) {
                    Ok(target) if target.is_absolute() => target,
                    Ok(target) => path.parent().unwrap_or_else(|| Path::new("/")).join(target),
                    Err(_) => return false,
                }
            }
        }
    };

    // Canonicalize root as well — intermediate path components (e.g. a home
    // directory exposed through a /home -> /usr/home symlink) would otherwise
    // make the starts_with prefix check fail even for a correct containment.
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolved.starts_with(canonical_root)
}

fn unlink_snapshot(
    state_dir: &Path,
    name: &str,
    kind: Kind,
    snapshot: &[PathBuf],
    preserve: &BTreeSet<PathBuf>,
) -> Result<()> {
    for link in snapshot {
        if preserve.contains(link) {
            continue;
        }
        if fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            fs::remove_file(link)?;
        }
    }

    // Clear the snapshot entries from the link-state file rather than
    // requiring byte-exact equality. The pre-fix code only cleared when
    // the on-disk state matched the snapshot exactly; if any entry in
    // the snapshot had been deleted externally (manual edit, partial
    // failure of a prior run, parallel install), the comparison failed
    // and the state file kept stale entries pointing at paths that no
    // longer exist. Recompute the remainder by filtering the snapshot
    // out of the current state and writing only what is left.
    let state_path = link_state::path(state_dir, name, kind);
    let current = link_state::read(&state_path)?;
    let snapshot_set: BTreeSet<&PathBuf> = snapshot
        .iter()
        .filter(|link| !preserve.contains(*link))
        .collect();
    let remainder: Vec<PathBuf> = current
        .into_iter()
        .filter(|link| !snapshot_set.contains(link))
        .collect();
    link_state::write(&state_path, &remainder)?;
    Ok(())
}

fn remove_stamps(state_dir: &Path, name: &str) -> Result<()> {
    cleanup::remove_stamps(state_dir, name, &mut cleanup::Summary::default())
}

fn remove_any_unless_preserved(path: &Path, preserve: &BTreeSet<PathBuf>) -> Result<()> {
    if preserve.contains(path) {
        return Ok(());
    }
    remove_any(path)
}

fn remove_any(path: &Path) -> Result<()> {
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
    Ok(())
}

fn remove_empty_install_parents(path: &Path, install_dir: &Path) -> Result<()> {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == install_dir || dir == Path::new("/") {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => break,
        }
        parent = dir.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use super::{Transition, cleanup_snapshot, points_into, unlink_snapshot};
    use crate::config::Entry;
    use crate::github_release_install::{self, ArchiveState};
    use crate::link_state::{self, Kind};
    use crate::manifest::ManifestEntry;
    use crate::runtime::Roots;
    use std::collections::BTreeSet;

    fn cleanup_snapshot_for_test(
        entry: &Entry,
        transition: &Transition,
        roots: &Roots,
    ) -> crate::Result<()> {
        let cleanup_roots = crate::cleanup::Roots {
            state_dir: roots.state_dir.clone(),
            install_dir: roots.install_dir.clone(),
            bin_dir: roots.bin_dir.clone(),
        };
        let repo_root = (transition.old.method == crate::method::GITHUB_REPO)
            .then(|| crate::cleanup::safe_repo_root(&transition.old, &cleanup_roots))
            .flatten();
        cleanup_snapshot(entry, transition, roots, repo_root.as_deref())
    }

    #[test]
    #[cfg(unix)]
    fn points_into_follows_direct_symlink() {
        let dir = temp_dir("direct");
        let target = dir.join("root/bin/tool");
        let link = dir.join("bin/tool");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"binary").unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        assert!(points_into(&link, &dir.join("root")));
        assert!(!points_into(&link, &dir.join("other")));
    }

    #[test]
    #[cfg(unix)]
    fn points_into_follows_chained_symlinks() {
        // Two-hop chain: bin/tool -> middle/tool -> root/bin/tool. The old
        // single-level read_link would resolve to `middle/tool` and then check
        // starts_with("root"), which is false — incorrectly treating the archive
        // install root as unowned and deleting it during a method transition.
        let dir = temp_dir("chained");
        let final_target = dir.join("root/bin/tool");
        let intermediate = dir.join("middle/tool");
        let link = dir.join("bin/tool");
        fs::create_dir_all(final_target.parent().unwrap()).unwrap();
        fs::write(&final_target, b"binary").unwrap();
        fs::create_dir_all(intermediate.parent().unwrap()).unwrap();
        symlink(&final_target, &intermediate).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&intermediate, &link).unwrap();

        assert!(points_into(&link, &dir.join("root")));
        assert!(!points_into(&link, &dir.join("middle")));
    }

    #[test]
    #[cfg(unix)]
    fn points_into_returns_false_for_broken_symlink() {
        let dir = temp_dir("broken");
        let link = dir.join("bin/tool");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(dir.join("nonexistent"), &link).unwrap();

        assert!(!points_into(&link, &dir.join("root")));
    }

    #[test]
    #[cfg(unix)]
    fn unlink_snapshot_clears_only_snapshot_entries_keeping_others_intact() {
        // The pre-fix code only cleared the state file when its
        // contents matched the snapshot byte-for-byte. If anything
        // had diverged externally — entries appended by a later
        // install, entries trimmed by manual cleanup, etc. — the
        // equality failed and the file kept stale snapshot entries
        // pointing at links that were just deleted. Filtering out
        // snapshot entries from the current state always converges
        // toward the correct remainder.
        let dir = temp_dir("clear-semantics");
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = link_state::path(&state_dir, "owner/tool", Kind::Extras);

        let snapshot = vec![dir.join("a"), dir.join("b")];
        // External addition: state file contains snapshot + an extra
        // link that does NOT belong to this transition.
        let extra = dir.join("external-other-dep-link");
        let mut on_disk = snapshot.clone();
        on_disk.push(extra.clone());
        link_state::write(&state_path, &on_disk).unwrap();

        // Pre-create the snapshot symlinks so the unlinker has
        // something to remove (its inner is_symlink check is also
        // exercised here).
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        for link in &snapshot {
            symlink(&target, link).unwrap();
        }
        symlink(&target, &extra).unwrap();

        unlink_snapshot(
            &state_dir,
            "owner/tool",
            Kind::Extras,
            &snapshot,
            &BTreeSet::new(),
        )
        .unwrap();

        let remaining = link_state::read(&state_path).unwrap();
        assert_eq!(
            remaining,
            vec![extra.clone()],
            "snapshot entries removed; foreign entries preserved"
        );
        assert!(extra.is_symlink(), "foreign link must still exist on disk");
    }

    #[test]
    #[cfg(unix)]
    fn unlink_snapshot_clears_state_even_when_external_entry_was_deleted() {
        // The complementary case: state file is missing one of the
        // snapshot entries (a prior partial cleanup removed it from
        // the file by hand). The byte-equality gate of the pre-fix
        // code would refuse to clear the file in this case, leaving
        // a stale entry pointing at a link the unlinker just removed.
        let dir = temp_dir("clear-with-missing");
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = link_state::path(&state_dir, "owner/tool", Kind::Extras);

        let snapshot = vec![dir.join("a"), dir.join("b")];
        // State file only has one of the two snapshot links.
        link_state::write(&state_path, std::slice::from_ref(&snapshot[0])).unwrap();
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        symlink(&target, &snapshot[0]).unwrap();

        unlink_snapshot(
            &state_dir,
            "owner/tool",
            Kind::Extras,
            &snapshot,
            &BTreeSet::new(),
        )
        .unwrap();

        // After clearing snapshot[0] from a state that only contained
        // snapshot[0], the file should be empty and (per
        // link_state::write semantics) removed entirely.
        assert!(
            !state_path.exists(),
            "state file should be removed once no entries remain"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_ignores_tampered_repo_install_path() {
        // The `github:repo` cleanup path once trusted the recorded manifest
        // path. A transition with a tampered
        // `old.install_path = /<bystander>` would therefore remove that
        // bystander. Cleanup authority now comes from the validated dependency
        // name and the checkout-lock root, so the recorded path is inert.
        //
        // Beyond the bystander assertion, this test also verifies that the
        // rest of the `github:repo` cleanup branch still runs. Ignoring the
        // recorded path must not skip legacy public-bin reconciliation or
        // stamp cleanup.
        let dir = temp_dir("tampered-install-path");
        let bystander = dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("data"), "user-owned").unwrap();

        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        // Per `cleanup_snapshot` for `github:repo`, the legacy public
        // bin lives at `<bin_dir>/<short_name>`. `short_name` of
        // `owner/tool` is `tool`, so seed that file and expect it to
        // be removed once cleanup runs to completion.
        let legacy_bin = roots.bin_dir.join("tool");
        fs::write(&legacy_bin, "old-bin").unwrap();

        // Stamps live next to the owner directory: `remove_stamps`
        // resolves `state_dir/<name>.parent()` → `state_dir/owner`
        // and removes `<base_name>.<kind>.stamp` files there.
        let stamp_dir = roots.state_dir.join("owner");
        fs::create_dir_all(&stamp_dir).unwrap();
        let stamp_file = stamp_dir.join("tool.repo.stamp");
        fs::write(&stamp_file, "stamp").unwrap();

        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                bystander.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(bystander.exists(), "bystander dir must be preserved");
        assert!(
            bystander.join("data").exists(),
            "bystander contents must be preserved"
        );
        // `legacy_bin` is `<bin_dir>/tool`, which equals
        // `preserve_paths` for the new `github:release` entry
        // (`bin_dir/<entry.cmd>` = `bin_dir/tool`). The preserve set
        // protects this file — that's the correct behavior because
        // the new method's freshly installed bin lives at that path.
        // So the file should remain, untouched, after cleanup.
        assert!(
            legacy_bin.exists(),
            "preserved public bin must not be removed during transition"
        );
        // The stamp belongs to the old method and is not preserved;
        // it must be removed by `remove_stamps`. If a regression skipped the
        // rest of cleanup while ignoring the tampered path, this assertion
        // would catch it.
        assert!(
            !stamp_file.exists(),
            "old-method stamp must be removed even when install_path is tampered"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_removes_binary_method_tracked_binlinks() {
        let dir = temp_dir("binary-binlinks");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        let target = dir.join("target");
        fs::write(&target, "old").unwrap();
        let public = roots.bin_dir.join("tool");
        let helper = roots.bin_dir.join("tool-helper");
        symlink(&target, &public).unwrap();
        symlink(&target, &helper).unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            &[public.clone(), helper.clone()],
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.to_string_lossy(),
            ),
            bin_links: vec![public.clone(), helper.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(!public.exists());
        assert!(!helper.exists());
        assert!(!link_state::path(&roots.state_dir, "owner/tool", Kind::Bin).exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_archive_regular_launcher() {
        let dir = temp_dir("preserve-archive-launcher");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let public = roots.bin_dir.join("tool");
        let install_root = roots.install_dir.join("owner/tool");
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        fs::write(&public, "user launcher").unwrap();
        fs::write(install_root.join("tool"), "archive binary").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&roots.install_dir, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.to_string_lossy(),
            ),
            bin_links: vec![public.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::Proven,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_to_string(&public).unwrap(), "user launcher");
        assert!(!install_root.exists());
        assert!(!link_state::path(&roots.state_dir, "owner/tool", Kind::Bin).exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_keeps_preserved_binlinks_in_state() {
        let dir = temp_dir("preserve-binlink-state");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        let target = dir.join("target");
        fs::write(&target, "new").unwrap();
        let public = roots.bin_dir.join("tool");
        let helper = roots.bin_dir.join("tool-helper");
        symlink(&target, &public).unwrap();
        symlink(&target, &helper).unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                roots.install_dir.join("owner/tool").to_string_lossy(),
            ),
            bin_links: vec![public.clone(), helper.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(public.exists());
        assert!(!helper.exists());
        assert_eq!(
            link_state::read(&link_state::path(&roots.state_dir, "owner/tool", Kind::Bin)).unwrap(),
            vec![public]
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_transition_preserves_unowned_regular_command() {
        use std::os::unix::fs::MetadataExt;

        let dir = temp_dir("repo-unowned-regular-command");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(install_root.join("bin")).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(install_root.join("bin/tool"), "managed command").unwrap();
        fs::write(&public, "generated client adapter").unwrap();
        let before = fs::metadata(&public).unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "generated client adapter"
        );
        let after = fs::metadata(&public).unwrap();
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        assert!(!install_root.exists());
    }

    #[test]
    fn repo_transition_cleanup_rejects_missing_lock_authority() {
        let dir = temp_dir("repo-transition-missing-lock");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("artifact"), "preserve\n").unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        let error = cleanup_snapshot(&new_entry, &transition, &roots, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires an acquired checkout-lock root")
        );
        assert!(install_root.join("artifact").exists());
    }

    #[test]
    #[cfg(unix)]
    fn repo_transition_preserves_new_root_through_symlinked_install_dir() {
        let dir = temp_dir("repo-transition-symlinked-install-root");
        let physical_install = dir.join("physical-install");
        let logical_install = dir.join("install-link");
        fs::create_dir_all(&physical_install).unwrap();
        symlink(&physical_install, &logical_install).unwrap();
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: logical_install.clone(),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let physical_root = physical_install.join("owner/tool");
        fs::create_dir_all(&physical_root).unwrap();
        fs::write(physical_root.join("new-artifact"), "preserve\n").unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                physical_root.display().to_string(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "cargo".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(
            fs::read_to_string(physical_root.join("new-artifact")).unwrap(),
            "preserve\n"
        );
        assert!(physical_install.exists());
        assert!(
            fs::symlink_metadata(logical_install)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_archive_transition_preserves_root_behind_regular_launcher() {
        let dir = temp_dir("archive-regular-launcher");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        fs::write(install_root.join("tool"), "archive binary").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&roots.install_dir, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        fs::write(&public, "user launcher").unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_to_string(&public).unwrap(), "user launcher");
        assert_eq!(
            fs::read_to_string(install_root.join("tool")).unwrap(),
            "archive binary"
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_archive_transition_uses_locked_root_after_install_alias_retarget() {
        let dir = temp_dir("archive-locked-root-retarget");
        let physical_a = dir.join("physical-a");
        let physical_b = dir.join("physical-b");
        let logical_install = dir.join("install-link");
        fs::create_dir_all(&physical_a).unwrap();
        fs::create_dir_all(&physical_b).unwrap();
        symlink(&physical_a, &logical_install).unwrap();

        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: logical_install.clone(),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let locked_root = physical_a.join("owner/tool");
        fs::create_dir_all(&locked_root).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(locked_root.join("tool"), "archive binary\n").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&physical_a, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        let public = roots.bin_dir.join("tool");
        fs::write(&public, "user launcher\n").unwrap();

        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                locked_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        fs::remove_file(&logical_install).unwrap();
        symlink(&physical_b, &logical_install).unwrap();

        cleanup_snapshot(&new_entry, &transition, &roots, Some(&locked_root)).unwrap();

        assert_eq!(
            fs::read_to_string(locked_root.join("tool")).unwrap(),
            "archive binary\n"
        );
        assert_eq!(fs::read_to_string(public).unwrap(), "user launcher\n");
        assert!(!physical_b.join("owner/tool").try_exists().unwrap());
        assert_eq!(fs::read_link(logical_install).unwrap(), physical_b);
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-transition-{name}"))
    }
}
