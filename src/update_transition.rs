//! Method-transition helpers for `shdeps update`.
//!
//! Update methods write new artifacts into the same public paths and state
//! files that old methods used. This module snapshots old ownership before the
//! install starts, then cleans only that snapshot after the new method succeeds.
//! Keeping the transaction-sensitive code here prevents each installer from
//! learning a partial version of the same migration rules.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cleanup;
use crate::config::{self, Entry};
use crate::link_state::{self, Kind};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::runtime::Roots;
use crate::update::Item;
use crate::Result;

/// Pre-install snapshot for a configured method transition.
#[derive(Debug, Clone)]
pub(crate) struct Transition {
    old: ManifestEntry,
    bin_links: Vec<PathBuf>,
    extra_links: Vec<PathBuf>,
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
            let bin_links =
                link_state::read(&link_state::path(&roots.state_dir, &entry.name, Kind::Bin))?;
            let extra_links = link_state::read(&link_state::path(
                &roots.state_dir,
                &entry.name,
                Kind::Extras,
            ))?;
            Ok((
                entry.name.clone(),
                Transition {
                    old: entry,
                    bin_links,
                    extra_links,
                },
            ))
        })
        .collect()
}

/// Returns the old manifest row for a transition.
pub(crate) fn old(transition: &Transition) -> &ManifestEntry {
    &transition.old
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
) -> Result<bool> {
    let Some(transition) = transition else {
        return Ok(false);
    };

    Ok(cleanup_snapshot(entry, transition, roots).is_err())
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

    if transition.old.method != "github:release"
        || transition.old.cmd != entry.cmd
        || !matches!(
            entry.method.as_str(),
            "github:repo" | "cargo" | "go" | "uv" | "npm"
        )
    {
        return Ok(None);
    }

    let original = roots.bin_dir.join(&entry.cmd);
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

fn cleanup_snapshot(entry: &Entry, transition: &Transition, roots: &Roots) -> Result<()> {
    let preserve = preserve_paths(entry, roots);

    match transition.old.method.as_str() {
        "pkg" => {
            // System packages are not shdeps-owned. Once another method has
            // succeeded, the manifest swap is enough; the OS package remains
            // available for anything else on the machine that might use it.
        }
        "github:repo" => {
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

            if !transition.old.install_path.is_empty() {
                let install_path = manifest_path(&transition.old.install_path, &roots.home);
                if !preserve.contains(&install_path) {
                    remove_any(&install_path)?;
                    remove_empty_install_parents(&install_path, &roots.install_dir)?;
                }
            }

            let legacy_bin = roots.bin_dir.join(config::short_name(&transition.old.name));
            remove_any_unless_preserved(&legacy_bin, &preserve)?;
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        "github:release" | "cargo" | "go" | "uv" | "npm" => {
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Extras,
                &transition.extra_links,
                &preserve,
            )?;
            remove_any_unless_preserved(&roots.bin_dir.join(&transition.old.cmd), &preserve)?;

            let install_root = roots.install_dir.join(&transition.old.name);
            if !preserve.contains(&install_root) {
                remove_any(&install_root)?;
                remove_empty_install_parents(&install_root, &roots.install_dir)?;
            }
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        "custom" => remove_stamps(&roots.state_dir, &transition.old.name)?,
        _ => {}
    }

    Ok(())
}

fn preserve_paths(entry: &Entry, roots: &Roots) -> BTreeSet<PathBuf> {
    let mut preserve = BTreeSet::new();

    // New-method link state may live at the same `<name>.links` path as the
    // old method. Read it after the install succeeds so transition cleanup can
    // remove only pre-existing links without deleting the freshly linked public
    // command, man page, or completion.
    for kind in [Kind::Bin, Kind::Extras] {
        if let Ok(links) = link_state::read(&link_state::path(&roots.state_dir, &entry.name, kind))
        {
            preserve.extend(links);
        }
    }

    match entry.method.as_str() {
        "github:repo" | "cargo" | "go" | "uv" | "npm" => {
            preserve.insert(roots.bin_dir.join(&entry.cmd));
            preserve.insert(roots.install_dir.join(&entry.name));
        }
        "github:release" => {
            preserve.insert(roots.bin_dir.join(&entry.cmd));

            let install_root = roots.install_dir.join(&entry.name);
            if release_install_root_is_owned(&install_root) {
                preserve.insert(install_root);
            }
        }
        _ => {}
    }

    preserve
}

fn release_install_root_is_owned(path: &Path) -> bool {
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
    // directory, however, is the archive payload we just installed and must be
    // kept. `symlink_metadata` is the important detail here: following symlinks
    // would make a stale dev-clone link look like an owned archive directory.
    metadata.is_dir() && !metadata.file_type().is_symlink()
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

    let state_path = link_state::path(state_dir, name, kind);
    if link_state::read(&state_path)? == snapshot {
        link_state::write(&state_path, &[])?;
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
