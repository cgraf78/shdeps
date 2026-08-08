//! Install helpers for `github:release` assets.
//!
//! This module owns the filesystem side of release installs. It is deliberately
//! separate from GitHub fetching and asset selection so tests can pin down
//! compatibility-sensitive ownership behavior without constructing fake HTTP
//! clients. Raw binaries intentionally preserve the historical Bash behavior
//! of writing directly to the public bin path; archive installs use a staged
//! directory because they own more than one filesystem object.

use std::fs;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::archive;
use crate::bin_link::{self, Link};
use crate::extras;
use crate::link_state::{self, Kind};
use crate::manifest;
use crate::method;
use crate::process;
use crate::state;

const ARCHIVE_LAYOUT_FILE: &str = ".shdeps-release-layout";
const ARCHIVE_LAYOUT_CONTENT: &str = "v1 archive\n";

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        // Extraction has not touched the live root until its final rename. A
        // best-effort scope guard therefore cleans every validation/error path
        // without risking the prior install or masking the primary failure.
        let _ = remove_any(&self.0);
    }
}

/// Evidence about a release archive root from the current filesystem state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveState {
    /// No Shdeps-owned archive root can be established.
    None,
    /// A marker or live managed symlink proves archive ownership.
    Proven,
    /// A legacy root exists beside an unowned public path, but old state cannot
    /// prove whether the root is current or stale.
    Ambiguous,
}

/// Returns the durable archive marker path for one dependency root.
pub(crate) fn archive_layout_path(install_base: &Path, name: &str) -> PathBuf {
    install_base.join(name).join(ARCHIVE_LAYOUT_FILE)
}

fn managed_install_dir(install_base: &Path, name: &str) -> Result<Option<PathBuf>> {
    let install_dir = install_base.join(name);
    let metadata = match fs::symlink_metadata(&install_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    Ok(Some(install_dir))
}

fn marker_state(install_dir: &Path) -> Result<ArchiveState> {
    let marker = install_dir.join(ARCHIVE_LAYOUT_FILE);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "release archive marker is not a regular file: {}",
                    marker.display()
                ),
            )
            .into())
        }
        Ok(_) => {
            let content = fs::read_to_string(&marker)?;
            if content != ARCHIVE_LAYOUT_CONTENT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown release archive marker: {}", content.trim()),
                )
                .into());
            }
            Ok(ArchiveState::Proven)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ArchiveState::None),
        Err(error) => Err(error.into()),
    }
}

/// Reads only explicit archive ownership for fail-closed recovery decisions.
///
/// A marker without a release manifest never authorizes deletion: a checkout
/// could contain the reserved path. Callers may use this evidence only to avoid
/// overwriting an interrupted archive install before the manifest was written.
pub(crate) fn explicit_archive_state(install_base: &Path, name: &str) -> Result<ArchiveState> {
    let Some(install_dir) = managed_install_dir(install_base, name)? else {
        return Ok(ArchiveState::None);
    };
    marker_state(&install_dir)
}

/// Classifies an archive root without following a symlink at the ownership
/// boundary. The marker deliberately records only that the root is an archive;
/// whether the public command is a Shdeps symlink or a user launcher can change
/// independently during a staggered dotfiles/Shdeps rollout.
pub(crate) fn archive_state(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
) -> Result<ArchiveState> {
    let Some(install_dir) = managed_install_dir(install_base, name)? else {
        return Ok(ArchiveState::None);
    };
    if marker_state(&install_dir)? == ArchiveState::Proven {
        return Ok(ArchiveState::Proven);
    }

    // Older archive installs predate the explicit marker. A real symlink into
    // this exact managed root is strong evidence that the directory is active.
    if symlink_points_into(public, &install_dir) {
        return Ok(ArchiveState::Proven);
    }

    if is_non_symlink(public) {
        // Secondary links prove only that an archive existed historically. Old
        // archive-to-raw conversion replaced the public command first and
        // cleared only bin-link state afterward, so a live man/completion link
        // can legitimately survive beside the new regular raw binary. Require
        // explicit launcher-owner intent before interpreting that shape as an
        // archive again.
        return Ok(ArchiveState::Ambiguous);
    }

    let bin_links = link_state::read(&link_state::path(state_dir, name, Kind::Bin))?;
    let extras_links = link_state::read(&link_state::path(state_dir, name, Kind::Extras))?;
    let has_archive_link = bin_links
        .iter()
        .chain(extras_links.iter())
        .any(|path| symlink_points_into(path, &install_dir));
    if has_archive_link {
        return Ok(ArchiveState::Proven);
    }

    Ok(ArchiveState::None)
}

/// Backfills the marker for a proven legacy archive during a mutating update.
pub(crate) fn repair_archive_marker(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
) -> Result<ArchiveState> {
    let marker = archive_layout_path(install_base, name);
    let archive = archive_state(state_dir, install_base, public, name)?;
    if archive == ArchiveState::Proven
        && fs::symlink_metadata(&marker).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        // This runs only from `update`, never status or prune. Keeping the
        // backfill on the mutating path avoids surprising writes from read-only
        // diagnostics while making subsequent cleanup unambiguous.
        state::write_atomic(&marker, ARCHIVE_LAYOUT_CONTENT)?;
    }
    Ok(archive)
}

/// Adopts one pre-marker archive after a caller deliberately replaces its
/// public symlink with a regular launcher.
///
/// The old bin-link ledger proves that Shdeps created this exact public path,
/// but it cannot by itself prove that the archive root is still current: an
/// interrupted archive-to-raw conversion can leave the same three filesystem
/// objects behind. Keep the generic classifier fail-closed and require this
/// explicit bridge call from the launcher owner to resolve that ambiguity.
pub(crate) fn adopt_legacy_archive_launcher(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
) -> Result<bool> {
    // This command runs immediately before a normal update but in a separate
    // process. Serialize classification and marker publication with that update
    // (and with timers or another pane) so a concurrent root swap cannot make us
    // stamp ownership onto a filesystem state we did not inspect.
    let _lock = state::StateLock::acquire(state_dir)?;

    let Some(install_dir) = managed_install_dir(install_base, name)? else {
        // Fresh installs have no legacy root to adopt. Treat that as success so
        // consumers can run one idempotent migration step on every machine.
        return Ok(true);
    };

    if marker_state(&install_dir)? == ArchiveState::Proven {
        // Repeated convergence is an idempotent success even after the original
        // manifest or public path changes; the co-activated marker is already
        // the stronger ownership record this migration exists to create.
        return Ok(true);
    }

    let public_is_regular = fs::symlink_metadata(public)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false);
    if !public_is_regular {
        return Ok(false);
    }

    let installed = manifest::read(&manifest::path(state_dir))?;
    let release_manifest = installed
        .get(name)
        .is_some_and(|entry| entry.method == method::GITHUB_RELEASE && entry.cmd == cmd);
    if !release_manifest {
        // A repo checkout uses the same stable root and historical link ledger.
        // Never turn it into a deletable release payload merely because a
        // consumer put a regular launcher in front of its command.
        return Ok(false);
    }

    let bin_links = link_state::read(&link_state::path(state_dir, name, Kind::Bin))?;
    let tracked_public = bin_links.iter().any(|path| path == public);
    let extras_links = link_state::read(&link_state::path(state_dir, name, Kind::Extras))?;
    let live_secondary_link = bin_links
        .iter()
        .chain(extras_links.iter())
        .any(|path| symlink_points_into(path, &install_dir));
    if !tracked_public && !live_secondary_link {
        return Ok(false);
    }

    state::write_atomic(
        &archive_layout_path(install_base, name),
        ARCHIVE_LAYOUT_CONTENT,
    )?;
    Ok(true)
}

pub(crate) fn is_non_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

pub(crate) fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn symlink_points_into(path: &Path, root: &Path) -> bool {
    if !fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return false;
    }
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

/// Installs a raw standalone release binary into `SHDEPS_BIN_DIR`.
pub fn install_plain(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    install_plain_to(&bin_dir.join(cmd), bytes)
}

/// Installs a raw standalone release binary to an exact caller-owned path.
pub(crate) fn install_plain_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(target);

    fs::write(&tmp, bytes)?;
    make_executable(&tmp)?;

    // `github:release` is the historical exception to shdeps' normal "never
    // overwrite regular files in SHDEPS_BIN_DIR" rule. Bash downloads/moves the
    // selected asset directly to the requested bin path, so preserve that
    // replacement behavior here and keep it isolated from the safer symlink
    // helpers used by repo, cargo, go, uv, and npm installs.
    fs::rename(&tmp, target)?;
    Ok(target.to_path_buf())
}

/// Installs a gzip-compressed standalone release binary.
pub fn install_gz(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    install_gz_to(&bin_dir.join(cmd), bytes)
}

/// Installs a gzip-compressed standalone release binary to an exact path.
pub(crate) fn install_gz_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;

    // Bash treats `.gz` release assets as compressed singles, not archives.
    // Reuse the plain-binary path after decompression so replacement and
    // executable-bit behavior stay identical to uncompressed release assets.
    install_plain_to(target, &decoded)
}

/// Installs a bzip2-compressed standalone release binary.
pub fn install_bz2(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    install_bz2_to(&bin_dir.join(cmd), bytes)
}

/// Installs a bzip2-compressed standalone release binary to an exact path.
pub(crate) fn install_bz2_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut decoder = bzip2::read::BzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;

    // Like `.gz`, Bash treats `.bz2` assets as compressed single binaries.
    // Keep the decompression-only difference isolated so raw release ownership
    // behavior has one implementation in `install_plain`.
    install_plain_to(target, &decoded)
}

/// Installs a zstd-compressed standalone release binary.
pub fn install_zst(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    install_zst_to(&bin_dir.join(cmd), bytes)
}

/// Installs an xz-compressed standalone release binary.
pub fn install_xz(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    install_xz_to(&bin_dir.join(cmd), bytes)
}

/// Installs an xz-compressed standalone release binary to an exact path.
pub(crate) fn install_xz_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut decoder = xz2::read::XzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;

    install_plain_to(target, &decoded)
}

/// Installs a zstd-compressed standalone release binary to an exact path.
pub(crate) fn install_zst_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let decoded = zstd::stream::decode_all(bytes)?;

    // `.zst` completes the Bash compressed-single behavior. Keep all public
    // bin ownership in `install_plain` so adding formats does not accidentally
    // drift from the raw release replacement contract.
    install_plain_to(target, &decoded)
}

/// Installs a gzip-compressed tar release archive.
pub fn install_tar_gz(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_tar_gz_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs a gzip-compressed tar archive and links to an exact public path.
pub(crate) fn install_tar_gz_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, false, |dest| {
        archive::unpack_tar_gz(bytes, dest).map(|_| ())
    })
}

/// Installs an uncompressed tar release archive.
pub fn install_tar(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_tar_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs an uncompressed tar archive and links to an exact public path.
pub(crate) fn install_tar_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, false, |dest| {
        archive::unpack_tar(bytes, dest).map(|_| ())
    })
}

/// Installs a bzip2-compressed tar release archive.
pub fn install_tar_bz2(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_tar_bz2_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs a bzip2-compressed tar archive and links to an exact public path.
pub(crate) fn install_tar_bz2_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, false, |dest| {
        archive::unpack_tar_bz2(bytes, dest).map(|_| ())
    })
}

/// Installs a zstd-compressed tar release archive.
pub fn install_tar_zst(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_tar_zst_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs a zstd-compressed tar archive and links to an exact public path.
pub(crate) fn install_tar_zst_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, false, |dest| {
        archive::unpack_tar_zst(bytes, dest).map(|_| ())
    })
}

/// Installs an xz-compressed tar release archive.
pub fn install_tar_xz(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_tar_xz_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs an xz-compressed tar archive and links to an exact public path.
pub(crate) fn install_tar_xz_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, false, |dest| {
        archive::unpack_tar_xz(bytes, dest).map(|_| ())
    })
}

/// Installs a zip release archive.
pub fn install_zip(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_zip_to(
        state_dir,
        install_base,
        &bin_dir.join(cmd),
        name,
        cmd,
        bytes,
    )
}

/// Installs a zip archive and links to an exact public path.
pub(crate) fn install_zip_to(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    install_archive(state_dir, install_base, public, name, cmd, true, |dest| {
        archive::unpack_zip(bytes, dest).map(|_| ())
    })
}

fn install_archive(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    cmd: &str,
    allow_non_executable_exact_binary: bool,
    extract: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<PathBuf> {
    let install_dir = install_base.join(name);
    let extract_dir = temp_install_path(&install_dir);
    remove_any(&extract_dir)?;
    let _extract_cleanup = RemoveOnDrop(extract_dir.clone());
    extract(&extract_dir)?;

    // Most GitHub release archives wrap their payload in a versioned top-level
    // directory. shdeps stores installs at a stable dependency path, so peel
    // that wrapper when it is unambiguous and leave multi-root archives intact.
    let content_root = content_root(&extract_dir)?;
    let binary =
        find_binary(&content_root, cmd, allow_non_executable_exact_binary).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{cmd} binary not found in release archive"),
            )
        })?;
    if !process::executable_path(&binary) {
        make_executable(&binary)?;
    }
    let relative_binary = binary
        .strip_prefix(&content_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(cmd));
    // The marker is part of the staged root, so the same atomic rename that
    // activates the archive also commits root ownership. It intentionally says
    // nothing about the mutable public command: a user launcher may replace a
    // Shdeps symlink, or vice versa, without changing who owns this payload.
    // Reserve the name rather than overwriting archive content: accepting an
    // upstream file here would make arbitrary payload data look like Shdeps
    // ownership metadata during later cleanup.
    let marker = content_root.join(ARCHIVE_LAYOUT_FILE);
    match fs::symlink_metadata(&marker) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "release archive contains reserved Shdeps metadata path: {}",
                    marker.display()
                ),
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::write(marker, ARCHIVE_LAYOUT_CONTENT)?;

    // The archive is fully extracted and validated before replacing the live
    // install. That keeps a bad download from destroying the currently working
    // tool, while still matching Bash's "latest install wins" behavior once we
    // know the new payload can provide the requested command.
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }

    // Backup/switch/rollback pattern (mirrors `release_activate::activate`):
    // the previous `remove_any(&install_dir)` immediately followed by
    // `rename(&content_root, &install_dir)` had a window where, if the
    // rename failed for any reason (transient FS error, permissions, a
    // stale file handle preventing the parent's directory entry from being
    // claimed), the existing install was already gone and the public
    // symlink left pointing at a now-missing path. Atomically rename the
    // old install to a sibling backup first, attempt the switch, and
    // restore the backup if the switch fails. Both renames stay on the
    // same filesystem (sibling paths) so they're atomic on POSIX.
    let backup = install_backup_path(&install_dir);
    // Do not use `Path::exists()` here: it follows symlinks, so a dangling
    // repo-install root would look absent even though its directory entry still
    // blocks the destination rename. The archive switch owns entries at this
    // boundary, not whatever a prior symlink happened to target.
    let had_existing = path_entry_exists(&install_dir)?;
    if had_existing {
        fs::rename(&install_dir, &backup)?;
    }
    if let Err(switch) = fs::rename(&content_root, &install_dir) {
        if had_existing {
            if let Err(rollback) = fs::rename(&backup, &install_dir) {
                // Two failures in a row: the live switch and restore both
                // failed. Keep the old backup for manual recovery, while the
                // extraction scope guard removes the failed candidate so a
                // retry cannot leak another full payload. Report both errors,
                // retaining the switch failure as the primary cause.
                return Err(std::io::Error::other(format!(
                    "archive install switch failed and backup restore also failed: \
                     switch={switch}, rollback={rollback}"
                ))
                .into());
            }
        }
        // Switch failed but previous install (if any) is restored.
        // Clean up the staged content directory so retries don't
        // accumulate `.tmp.<pid>`-style stragglers next to the live
        // install — same hygiene `release_activate` applies.
        let _ = remove_any(&content_root);
        return Err(switch.into());
    }
    let source = install_dir.join(relative_binary);
    if let Err(link) = replace_symlink(&source, public) {
        // Root activation and public-link publication are one transaction. If
        // the link cannot be committed, remove the new root and restore the old
        // one before the caller restores any parked raw public command. Leaving
        // a marked new root beside that old command would make a retry mistake
        // the raw binary for a deliberately preserved launcher.
        if let Err(remove) = remove_any(&install_dir) {
            return Err(io::Error::other(format!(
                "archive public link failed and new root removal also failed: link={link}, remove={remove}"
            ))
            .into());
        }
        if had_existing {
            if let Err(rollback) = fs::rename(&backup, &install_dir) {
                return Err(io::Error::other(format!(
                    "archive public link failed and root restore also failed: link={link}, restore={rollback}"
                ))
                .into());
            }
        }
        return Err(link);
    }
    if had_existing {
        // The root and its public command are now committed. Backup cleanup is
        // best-effort so an antivirus or transient handle does not turn a good
        // install into a manifest-less failure. `remove_any` uses
        // `symlink_metadata`, so a parked symlink is unlinked, never followed.
        let _ = remove_any(&backup);
    }
    // Bin-dir fanout remains best-effort: the co-activated layout marker now
    // carries archive ownership even when a regular launcher was preserved.
    let _ = link_archive_bins(state_dir, public, name, &install_dir);
    // Release archives commonly carry completions or man pages beside the
    // binary. Reusing the shared extras linker keeps those secondary artifacts
    // tracked and prunable exactly like repo-based installs.
    //
    // Extras linking is best-effort: a failure here (rare permission or
    // state-dir error) must not undo a successfully installed binary. The
    // binary symlink at `public` is already live; returning Err now would leave
    // the dep installed but with no manifest entry, causing a spurious
    // reinstall on every future `shdeps update`.
    let _ = extras::link(state_dir, install_base, name, &install_dir);
    Ok(public.to_path_buf())
}

fn link_archive_bins(
    state_dir: &Path,
    public: &Path,
    name: &str,
    install_dir: &Path,
) -> Result<()> {
    let Some(public_bin_dir) = public.parent() else {
        return Ok(());
    };
    if public_bin_dir.starts_with(install_dir) || !public_bin_dir.is_dir() {
        return Ok(());
    }
    clear_archive_bin_links(state_dir, name, public)?;

    let source_dir = install_dir.join("bin");
    let Ok(entries) = fs::read_dir(&source_dir) else {
        return Ok(());
    };

    let mut sources = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !process::executable_path(&path) {
            continue;
        }
        let Some(cmd) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        sources.push((cmd.to_owned(), path));
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));

    let mut created = Vec::new();
    for (cmd, path) in sources {
        let target = public_bin_dir.join(&cmd);
        if target == public {
            if symlink_points_into(public, install_dir) {
                created.push(public.to_path_buf());
            }
        } else if let Link::Linked(link) = bin_link::one(public_bin_dir, &cmd, &path)? {
            created.push(link);
        }
    }
    if !created.is_empty()
        && !created.iter().any(|link| link == public)
        && symlink_points_into(public, install_dir)
    {
        created.push(public.to_path_buf());
    }
    created.sort();

    link_state::write(&link_state::path(state_dir, name, Kind::Bin), &created)?;
    Ok(())
}

pub(crate) fn clear_archive_bin_links(state_dir: &Path, name: &str, preserve: &Path) -> Result<()> {
    let state_path = link_state::path(state_dir, name, Kind::Bin);
    for link in link_state::read(&state_path)? {
        if link == preserve {
            continue;
        }
        if fs::symlink_metadata(&link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            fs::remove_file(link)?;
        }
    }
    link_state::write(&state_path, &[])?;
    Ok(())
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

fn temp_install_path(target: &Path) -> PathBuf {
    let mut tmp = target.to_path_buf();
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("release-install");
    tmp.set_file_name(format!(".{name}.tmp.{}", std::process::id()));
    tmp
}

/// Sibling path used to atomically park the prior install_dir while a
/// new archive is being renamed into place. Same shape as
/// `release_activate::backup_path` — kept local to avoid cross-module
/// dependency on a 3-line helper, but the structural intent is
/// identical (sibling path on the same filesystem so the rename is
/// atomic on POSIX, with `pid + nanos` to avoid collision between
/// concurrent installs targeting the same parent dir).
fn install_backup_path(install_dir: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    install_dir.with_extension(format!(
        "shdeps-archive-backup-{}-{nanos}",
        std::process::id()
    ))
}

fn content_root(extract_dir: &Path) -> Result<PathBuf> {
    let entries = fs::read_dir(extract_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() == 1 {
        let path = entries[0].path();
        if path.is_dir() {
            return Ok(path);
        }
    }
    Ok(extract_dir.to_path_buf())
}

fn find_binary(root: &Path, cmd: &str, allow_non_executable_exact_binary: bool) -> Option<PathBuf> {
    let mut prefixed = None;
    let mut non_executable_exact = None;
    for path in walk_files(root) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == cmd {
            if process::executable_path(&path) {
                return Some(path);
            }
            if allow_non_executable_exact_binary && non_executable_exact.is_none() {
                non_executable_exact = Some(path);
            }
            continue;
        }
        if !process::executable_path(&path) {
            continue;
        }
        // Some projects ship platform-suffixed binaries inside a generic
        // archive. Prefer the exact command when present, but keep the first
        // executable `cmd-*`/`cmd_*` fallback without guessing unrelated
        // filenames.
        if prefixed.is_none()
            && (name.starts_with(&format!("{cmd}-")) || name.starts_with(&format!("{cmd}_")))
        {
            prefixed = Some(path);
        }
    }
    prefixed.or(non_executable_exact)
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // The archive extractor rejects symlinks and hardlinks before this
            // walk runs, so recursive descent cannot escape the staged tree.
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
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

#[cfg(unix)]
fn replace_symlink(source: &Path, target: &Path) -> Result<bool> {
    if source == target {
        // Custom hooks can deliberately place the public command at the binary
        // path inside the managed install tree. Dotfiles' Neovim hook does this
        // so ~/.local/bin/nvim can remain a launcher while the real editor
        // lives at ~/.local/share/neovim/neovim/bin/nvim. In that layout there
        // is nothing to link: replacing `target` would first remove the real
        // binary, then create a self-referential symlink that can never exec.
        return Ok(false);
    }

    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "release public-bin target has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    // Archive installs follow the same public-path ownership rule as every
    // other symlink-based method: replace a Shdeps-owned symlink, but preserve
    // a regular launcher. Raw and compressed-single release assets retain the
    // historical replacement behavior in `install_plain_to` above.
    extras::replace_symlink(source, target)
}

#[cfg(not(unix))]
fn replace_symlink(source: &Path, target: &Path) -> Result<bool> {
    if source == target {
        return Ok(false);
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if is_non_symlink(target) {
        return Ok(false);
    }
    fs::copy(source, target)?;
    Ok(true)
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
    use std::io::Cursor;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use bzip2::Compression as BzCompression;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use crate::link_state::{self, Kind};

    #[test]
    #[cfg(unix)]
    fn archive_marker_never_follows_a_symlinked_install_root() {
        let dir = temp_dir("symlinked-archive-root");
        let external = dir.join("external-checkout");
        let install_base = dir.join("share");
        let public = dir.join("bin/tool");
        fs::create_dir_all(&external).unwrap();
        fs::create_dir_all(install_base.join("owner")).unwrap();
        fs::write(
            external.join(super::ARCHIVE_LAYOUT_FILE),
            "external sentinel\n",
        )
        .unwrap();
        symlink(&external, install_base.join("owner/tool")).unwrap();

        assert_eq!(
            super::explicit_archive_state(&install_base, "owner/tool").unwrap(),
            super::ArchiveState::None
        );
        assert_eq!(
            super::repair_archive_marker(&dir.join("state"), &install_base, &public, "owner/tool")
                .unwrap(),
            super::ArchiveState::None
        );
        assert_eq!(
            fs::read_to_string(external.join(super::ARCHIVE_LAYOUT_FILE)).unwrap(),
            "external sentinel\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn legacy_archive_extra_requires_explicit_launcher_adoption() {
        let dir = temp_dir("legacy-archive-extra-proof");
        let state_dir = dir.join("state");
        let install_base = dir.join("share");
        let install_dir = install_base.join("owner/tool");
        let public = dir.join("bin/tool");
        let man_link = install_base.join("man/man1/tool.1");
        let man_source = install_dir.join("share/man/man1/tool.1");
        fs::create_dir_all(man_source.parent().unwrap()).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::create_dir_all(man_link.parent().unwrap()).unwrap();
        fs::write(&man_source, "manual").unwrap();
        fs::write(&public, "#!/bin/sh\nexec real-hm \"$@\"\n").unwrap();
        symlink(&man_source, &man_link).unwrap();
        link_state::write(
            &link_state::path(&state_dir, "owner/tool", Kind::Extras),
            std::slice::from_ref(&man_link),
        )
        .unwrap();
        fs::write(
            crate::manifest::path(&state_dir),
            format!(
                "owner/tool|github:release|tool|{}\n",
                install_dir.join("bin/tool").display()
            ),
        )
        .unwrap();

        assert_eq!(
            super::repair_archive_marker(&state_dir, &install_base, &public, "owner/tool").unwrap(),
            super::ArchiveState::Ambiguous
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());
        assert!(
            super::adopt_legacy_archive_launcher(
                &state_dir,
                &install_base,
                &public,
                "owner/tool",
                "tool"
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(super::archive_layout_path(&install_base, "owner/tool")).unwrap(),
            "v1 archive\n"
        );
    }

    #[test]
    fn corrupt_archive_marker_fails_closed() {
        let dir = temp_dir("corrupt-archive-marker");
        let install_base = dir.join("share");
        let marker = super::archive_layout_path(&install_base, "owner/tool");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, "future format\n").unwrap();

        let error = super::archive_state(
            &dir.join("state"),
            &install_base,
            &dir.join("bin/tool"),
            "owner/tool",
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown release archive marker"));
        assert!(marker.parent().unwrap().exists());
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_rejects_reserved_marker_from_upstream_payload() {
        let dir = temp_dir("reserved-archive-marker");
        let public = dir.join("bin/tool");
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(&public, "user launcher").unwrap();
        let bytes = tar_gz(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            (
                "tool-v1.0/.shdeps-release-layout",
                b"v1 archive\n".as_slice(),
                0o644,
            ),
        ]);

        let error = super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap_err();

        assert!(error.to_string().contains("reserved Shdeps metadata path"));
        assert_eq!(fs::read_to_string(public).unwrap(), "user launcher");
        assert!(!dir.join("share/owner/tool").exists());
        assert!(
            fs::read_dir(dir.join("share/owner"))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "rejected archive must not leave an extracted staging tree"
        );
    }

    #[test]
    fn unmarked_root_with_regular_public_path_is_ambiguous() {
        let dir = temp_dir("ambiguous-archive-root");
        let install_base = dir.join("share");
        let public = dir.join("bin/tool");
        fs::create_dir_all(install_base.join("owner/tool")).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(&public, "old command").unwrap();

        assert_eq!(
            super::archive_state(&dir.join("state"), &install_base, &public, "owner/tool").unwrap(),
            super::ArchiveState::Ambiguous
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());
        assert_eq!(fs::read_to_string(public).unwrap(), "old command");
    }

    #[test]
    fn interrupted_archive_to_raw_state_stays_ambiguous() {
        let dir = temp_dir("interrupted-archive-to-raw");
        let state_dir = dir.join("state");
        let install_base = dir.join("share");
        let install_dir = install_base.join("owner/tool");
        let public = dir.join("bin/tool");
        fs::create_dir_all(install_dir.join("bin")).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(install_dir.join("bin/tool"), "stale archive binary").unwrap();
        fs::write(&public, "raw release binary").unwrap();
        link_state::write(
            &link_state::path(&state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();

        // Old Shdeps installed a raw asset before best-effort clearing the
        // prior archive ledger. A crash between those steps is indistinguishable
        // from launcher adoption without explicit caller intent, so the generic
        // classifier must continue to fail closed.
        assert_eq!(
            super::repair_archive_marker(&state_dir, &install_base, &public, "owner/tool").unwrap(),
            super::ArchiveState::Ambiguous
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());
        assert_eq!(fs::read_to_string(public).unwrap(), "raw release binary");
    }

    #[test]
    fn explicit_legacy_launcher_adoption_backfills_marker() {
        let dir = temp_dir("legacy-bin-link-launcher-migration");
        let state_dir = dir.join("state");
        let install_base = dir.join("share");
        let install_dir = install_base.join("owner/tool");
        let public = dir.join("bin/tool");
        fs::create_dir_all(install_dir.join("bin")).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(install_dir.join("bin/tool"), "old archive binary").unwrap();
        fs::write(&public, "tracked launcher").unwrap();
        link_state::write(
            &link_state::path(&state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        fs::write(
            crate::manifest::path(&state_dir),
            format!(
                "owner/tool|github:release|tool|{}\n",
                install_dir.join("bin/tool").display()
            ),
        )
        .unwrap();

        assert!(
            super::adopt_legacy_archive_launcher(
                &state_dir,
                &install_base,
                &public,
                "owner/tool",
                "tool"
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(super::archive_layout_path(&install_base, "owner/tool")).unwrap(),
            "v1 archive\n"
        );
        assert_eq!(fs::read_to_string(public).unwrap(), "tracked launcher");
    }

    #[test]
    #[cfg(unix)]
    fn explicit_launcher_adoption_rejects_repo_roots_and_non_regular_paths() {
        let dir = temp_dir("reject-invalid-launcher-adoption");
        let state_dir = dir.join("state");
        let install_base = dir.join("share");
        let install_dir = install_base.join("owner/tool");
        let public = dir.join("bin/tool");
        fs::create_dir_all(install_dir.join("bin")).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(install_dir.join("bin/tool"), "repo binary").unwrap();
        fs::write(&public, "tracked launcher").unwrap();
        link_state::write(
            &link_state::path(&state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let manifest_path = crate::manifest::path(&state_dir);
        fs::write(
            &manifest_path,
            format!("owner/tool|github:repo|tool|{}\n", install_dir.display()),
        )
        .unwrap();

        assert!(
            !super::adopt_legacy_archive_launcher(
                &state_dir,
                &install_base,
                &public,
                "owner/tool",
                "tool"
            )
            .unwrap()
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());

        fs::remove_file(&public).unwrap();
        symlink(install_dir.join("bin/tool"), &public).unwrap();
        assert!(
            !super::adopt_legacy_archive_launcher(
                &state_dir,
                &install_base,
                &public,
                "owner/tool",
                "tool"
            )
            .unwrap()
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());

        fs::write(
            &manifest_path,
            format!(
                "owner/tool|github:release|tool|{}\n",
                install_dir.join("bin/tool").display()
            ),
        )
        .unwrap();
        fs::remove_file(&public).unwrap();
        fs::create_dir(&public).unwrap();

        assert!(
            !super::adopt_legacy_archive_launcher(
                &state_dir,
                &install_base,
                &public,
                "owner/tool",
                "tool"
            )
            .unwrap()
        );
        assert!(!super::archive_layout_path(&install_base, "owner/tool").exists());
    }

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

    #[test]
    #[cfg(unix)]
    fn gz_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("gz-single");
        let bytes = gzip(b"binary");

        let target = super::install_gz(&dir, "tool", &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn bz2_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("bz2-single");
        let bytes = bzip2(b"binary");

        let target = super::install_bz2(&dir, "tool", &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn xz_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("xz-single");
        let bytes = xz(b"binary");

        let target = super::install_xz(&dir, "tool", &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn zst_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("zst-single");
        let bytes = zstd(b"binary");

        let target = super::install_zst(&dir, "tool", &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn tar_gz_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("tar-gz");
        let bytes = tar_gz(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/bin/tool-helper", b"helper".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);

        let public = super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("bin/tool-helper")).unwrap(),
            dir.join("share/owner/tool/bin/tool-helper")
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
        assert!(fs::read_dir(dir.join("share/owner")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
    }

    #[test]
    #[cfg(unix)]
    fn archive_bin_links_remove_stale_commands_on_update() {
        let dir = temp_dir("archive-bin-stale");
        let bytes_v1 = tar_gz(&[
            ("tool-v1.0/bin/tool", b"v1".as_slice(), 0o755),
            ("tool-v1.0/bin/old-helper", b"old".as_slice(), 0o755),
        ]);
        let bytes_v2 = tar_gz(&[
            ("tool-v2.0/bin/tool", b"v2".as_slice(), 0o755),
            ("tool-v2.0/bin/new-helper", b"new".as_slice(), 0o755),
        ]);

        super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();
        assert!(dir.join("bin/old-helper").is_symlink());

        super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes_v2,
        )
        .unwrap();

        assert!(dir.join("bin/tool").is_symlink());
        assert!(!dir.join("bin/old-helper").exists());
        assert_eq!(
            fs::read_link(dir.join("bin/new-helper")).unwrap(),
            dir.join("share/owner/tool/bin/new-helper")
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_bin_link_cleanup_keeps_state_limited_to_actual_symlinks() {
        let dir = temp_dir("archive-bin-to-top-level");
        let bytes_v1 = tar_gz(&[
            ("tool-v1.0/bin/tool", b"v1".as_slice(), 0o755),
            ("tool-v1.0/bin/tool-helper", b"helper".as_slice(), 0o755),
        ]);
        let bytes_v2 = tar_gz(&[("tool-v2.0/tool", b"v2".as_slice(), 0o755)]);

        super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();
        assert!(dir.join("bin/tool").is_symlink());
        assert!(dir.join("bin/tool-helper").is_symlink());

        super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes_v2,
        )
        .unwrap();

        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/tool")
        );
        assert!(!dir.join("bin/tool-helper").exists());
        assert_eq!(
            link_state::read(&dir.join("state/owner/tool.binlinks")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_binlinks_include_configured_command_from_outside_bin_when_helpers_exist() {
        let dir = temp_dir("archive-top-level-command-with-helper");
        let bytes = tar_gz(&[
            ("tool-v1.0/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/bin/tool-helper", b"helper".as_slice(), 0o755),
        ]);

        super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(
            crate::link_state::read(&crate::link_state::path(
                &dir.join("state"),
                "owner/tool",
                crate::link_state::Kind::Bin
            ))
            .unwrap(),
            vec![dir.join("bin/tool"), dir.join("bin/tool-helper")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn tar_gz_install_keeps_in_tree_custom_public_binary() {
        let dir = temp_dir("tar-gz-in-tree-public");
        let bytes = tar_gz(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);
        let public = dir.join("share/owner/tool/bin/tool");

        let installed = super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(installed, public);
        assert_eq!(fs::read(&public).unwrap(), b"binary");
        assert!(
            !fs::symlink_metadata(&public)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn tar_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("tar");
        let bytes = tar(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);

        let public = super::install_tar(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_preserves_regular_public_launcher_across_updates() {
        let dir = temp_dir("archive-public-launcher");
        let public = dir.join("bin/tool");
        let bytes_v1 = tar_gz(&[
            ("tool-v1.0/bin/tool", b"v1".as_slice(), 0o755),
            ("tool-v1.0/bin/tool-helper", b"helper-v1".as_slice(), 0o755),
        ]);
        let bytes_v2 = tar_gz(&[
            ("tool-v2.0/bin/tool", b"v2".as_slice(), 0o755),
            ("tool-v2.0/bin/tool-helper", b"helper-v2".as_slice(), 0o755),
        ]);

        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(&public, b"user launcher").unwrap();
        let mut permissions = fs::metadata(&public).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&public, permissions).unwrap();

        for (bytes, expected) in [(&bytes_v1, b"v1"), (&bytes_v2, b"v2")] {
            super::install_tar_gz_to(
                &dir.join("state"),
                &dir.join("share"),
                &public,
                "owner/tool",
                "tool",
                bytes,
            )
            .unwrap();

            assert_eq!(fs::read(&public).unwrap(), b"user launcher");
            assert!(
                !fs::symlink_metadata(&public)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read(dir.join("share/owner/tool/bin/tool")).unwrap(),
                expected
            );
            assert_eq!(
                link_state::read(&link_state::path(
                    &dir.join("state"),
                    "owner/tool",
                    Kind::Bin,
                ))
                .unwrap(),
                vec![dir.join("bin/tool-helper")]
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_uses_backup_swap_and_cleans_up() {
        // Regression for the iteration-3 codex finding that
        // `install_archive` used to `remove_any(install_dir)` BEFORE
        // the final rename, leaving a window where a failed rename
        // would strand the user with no install at all. The new flow
        // moves the existing install to a sibling backup, renames the
        // staged content into place, and only then removes the
        // backup. The happy-path observable is: existing install is
        // replaced AND no `.<name>.shdeps-archive-backup-*` sibling
        // remains afterward. The adjacent test covers rollback after
        // the root switch succeeds but public-link publication fails.
        let dir = temp_dir("archive-install-backup-swap");
        let bytes_v1 = tar_gz(&[("tool-v1.0/bin/tool", b"v1".as_slice(), 0o755)]);
        let bytes_v2 = tar_gz(&[("tool-v2.0/bin/tool", b"v2".as_slice(), 0o755)]);
        let public = dir.join("bin/tool");

        // First install establishes a baseline so the second install
        // exercises the backup-then-replace branch (not the
        // had_existing=false branch).
        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes_v2,
        )
        .unwrap();

        // The new install is live.
        assert_eq!(fs::read(public.canonicalize().unwrap()).unwrap(), b"v2");

        // And no backup directory is left next to the live install.
        // The backup name pattern is `.<install_dir>.shdeps-archive-
        // backup-<pid>-<nanos>` placed as a sibling of `install_dir`.
        let install_parent = dir.join("share/owner");
        let backups: Vec<_> = fs::read_dir(&install_parent)
            .unwrap()
            .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
            .filter(|n| n.contains(".shdeps-archive-backup-"))
            .collect();
        assert!(
            backups.is_empty(),
            "no backup dir should remain after successful install, found: {backups:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_rolls_back_root_when_public_link_fails() {
        let dir = temp_dir("archive-public-link-rollback");
        let state_dir = dir.join("state");
        let install_base = dir.join("share");
        let install_dir = install_base.join("owner/tool");
        let original_public = dir.join("bin/tool");
        let bytes_v1 = tar_gz(&[("tool-v1.0/bin/tool", b"v1".as_slice(), 0o755)]);
        let bytes_v2 = tar_gz(&[("tool-v2.0/bin/tool", b"v2".as_slice(), 0o755)]);

        super::install_tar_gz_to(
            &state_dir,
            &install_base,
            &original_public,
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();
        let original_link = fs::read_link(&original_public).unwrap();
        let marker = install_dir.join(super::ARCHIVE_LAYOUT_FILE);
        let original_marker = fs::read(&marker).unwrap();

        // A regular file where the new public path needs a directory lets the
        // staged root activate successfully and then fails publication in the
        // shared link helper. This exercises the transactional rollback branch
        // without permissions or platform-specific fault injection.
        let blocker = dir.join("blocked");
        fs::write(&blocker, "sentinel").unwrap();
        let error = super::install_tar_gz_to(
            &state_dir,
            &install_base,
            &blocker.join("tool"),
            "owner/tool",
            "tool",
            &bytes_v2,
        )
        .unwrap_err();

        assert!(!error.to_string().is_empty());
        assert_eq!(fs::read(install_dir.join("bin/tool")).unwrap(), b"v1");
        assert_eq!(fs::read(marker).unwrap(), original_marker);
        assert_eq!(fs::read_link(&original_public).unwrap(), original_link);
        assert_eq!(
            fs::read(original_public.canonicalize().unwrap()).unwrap(),
            b"v1"
        );
        assert_eq!(fs::read_to_string(blocker).unwrap(), "sentinel");

        let leftovers: Vec<_> = fs::read_dir(install_dir.parent().unwrap())
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().into_string().ok())
            })
            .filter(|name| {
                name.contains(".shdeps-archive-backup-") || name.starts_with(".tool.tmp.")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "rollback must not leave staged or backup roots: {leftovers:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_replaces_dangling_install_root_symlink() {
        // A repo-based install can leave its stable root as a symlink after the
        // checkout it targeted is removed. Path::exists follows that dangling
        // link and reports false, but the directory entry still occupies the
        // archive destination. Treating it as absent would make every retry
        // fail at the final rename instead of converging to the release.
        let dir = temp_dir("archive-replaces-dangling-root");
        let install_dir = dir.join("share/owner/tool");
        let public = dir.join("bin/tool");
        fs::create_dir_all(install_dir.parent().unwrap()).unwrap();
        symlink(dir.join("removed-checkout"), &install_dir).unwrap();

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &tar_gz(&[("tool-v1.0/bin/tool", b"release".as_slice(), 0o755)]),
        )
        .unwrap();

        assert!(install_dir.is_dir());
        assert!(!install_dir.is_symlink());
        assert_eq!(
            fs::read(public.canonicalize().unwrap()).unwrap(),
            b"release"
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_reuses_atomic_public_link_helper() {
        // Archive installs now share `extras::replace_symlink` with the other
        // managed-root methods. Exercise replacement of a dangling link and
        // assert the shared helper's staging namespace is empty afterward so a
        // failed rename cleanup cannot silently accumulate beside public bins.
        let dir = temp_dir("atomic-rename-release-link");
        let bytes = tar_gz(&[("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755)]);
        let public = dir.join("bin/tool");

        // Stage a dangling symlink at the target so the path already
        // has an entry that needs to be replaced.
        fs::create_dir_all(dir.join("bin")).unwrap();
        std::os::unix::fs::symlink(dir.join("nonexistent"), &public).unwrap();

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        let leftovers: Vec<_> = fs::read_dir(dir.join("bin"))
            .unwrap()
            .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
            .filter(|n| n.starts_with(".tool.shdeps-link."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no staging file should remain after successful atomic rename, found: {leftovers:?}"
        );
        // The new symlink resolves and the dangling one is gone.
        assert!(public.is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn archive_install_succeeds_even_when_extras_link_fails() {
        // Place a regular file where state_dir should be. link_state operations
        // expect a directory and fail with ENOTDIR when they try to read or
        // write link-state files beneath it. This simulates the unlikely but
        // possible case where state writes fail (permissions, disk full, etc.)
        // after the binary has already been extracted and symlinked. The install
        // must still return Ok so the caller can write the manifest entry and
        // avoid a spurious reinstall on the next update.
        let dir = temp_dir("archive-extras-fail");
        let bytes = tar_gz(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);
        let state_as_file = dir.join("state");
        fs::write(&state_as_file, "blocker").unwrap();

        let public = super::install_tar_gz(
            &state_as_file,
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
    }

    #[test]
    #[cfg(unix)]
    fn archive_marker_keeps_preserved_launcher_independent_of_link_state() {
        let dir = temp_dir("archive-launcher-marker");
        let bytes = tar_gz(&[("tool-v1.0/tool", b"binary".as_slice(), 0o755)]);
        let state_as_file = dir.join("state");
        let public = dir.join("bin/tool");
        fs::write(&state_as_file, "blocker").unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(&public, "user launcher").unwrap();
        let mut permissions = fs::metadata(&public).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&public, permissions).unwrap();

        super::install_tar_gz_to(
            &state_as_file,
            &dir.join("share"),
            &public,
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&public).unwrap(), "user launcher");
        assert_eq!(
            fs::read_to_string(super::archive_layout_path(&dir.join("share"), "owner/tool"))
                .unwrap(),
            "v1 archive\n"
        );
        assert_eq!(fs::read_to_string(&state_as_file).unwrap(), "blocker");
    }

    #[test]
    fn tar_gz_install_rejects_archives_without_matching_binary() {
        let dir = temp_dir("missing");
        let bytes = tar_gz(&[("tool-v1.0/README.md", b"readme".as_slice(), 0o644)]);

        let error = super::install_tar_gz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap_err();

        assert!(error.to_string().contains("tool binary not found"));
        assert!(
            fs::read_dir(dir.join("share/owner"))
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "invalid archive must not leave an extracted staging tree"
        );
    }

    #[test]
    #[cfg(unix)]
    fn tar_bz2_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("tar-bz2");
        let bytes = tar_bz2(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);

        let public = super::install_tar_bz2(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn tar_zst_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("tar-zst");
        let bytes = tar_zst(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);

        let public = super::install_tar_zst(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn tar_xz_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("tar-xz");
        let bytes = tar_xz(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.0/share/man/man1/tool.1", b"man".as_slice(), 0o644),
        ]);

        let public = super::install_tar_xz(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("share/man/man1/tool.1")).unwrap(),
            dir.join("share/owner/tool/share/man/man1/tool.1")
        );
    }

    #[test]
    #[cfg(unix)]
    fn zip_install_descends_single_root_links_binary_and_extras() {
        let dir = temp_dir("zip");
        let bytes = zip(&[
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
            (
                "tool-v1.0/share/zsh/site-functions/_tool",
                b"comp".as_slice(),
                0o644,
            ),
        ]);

        let public = super::install_zip(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();

        assert_eq!(public, dir.join("bin/tool"));
        assert_eq!(
            fs::read_link(dir.join("bin/tool")).unwrap(),
            dir.join("share/owner/tool/bin/tool")
        );
        assert_eq!(
            fs::read_link(dir.join("share/zsh/site-functions/_tool")).unwrap(),
            dir.join("share/owner/tool/share/zsh/site-functions/_tool")
        );
    }

    #[test]
    #[cfg(unix)]
    fn zip_install_accepts_exact_binary_without_unix_mode_bits() {
        let dir = temp_dir("zip-no-mode");
        let bytes = zip(&[("tool-v1.0/bin/tool", b"binary".as_slice(), 0o644)]);

        let public = super::install_zip(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();
        let target = public.canonicalize().unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn zip_install_prefers_executable_exact_binary_over_non_executable_collateral() {
        let dir = temp_dir("zip-executable-exact");
        let bytes = zip(&[
            ("tool-v1.0/docs/tool", b"docs".as_slice(), 0o644),
            ("tool-v1.0/bin/tool", b"binary".as_slice(), 0o755),
        ]);

        let public = super::install_zip(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin"),
            "owner/tool",
            "tool",
            &bytes,
        )
        .unwrap();
        let target = public.canonicalize().unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"binary");
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn bzip2(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), BzCompression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 0).unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_bz2(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        bzip2(&tar(entries))
    }

    fn tar_zst(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        zstd(&tar(entries))
    }

    fn tar_xz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        xz(&tar(entries))
    }

    fn tar(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for (path, body, mode) in entries {
                let mut header = Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append(&header, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        tar
    }

    fn zip(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = Cursor::new(&mut bytes);
            let mut writer = ZipWriter::new(cursor);
            for (path, body, mode) in entries {
                let options = SimpleFileOptions::default().unix_permissions(*mode);
                writer.start_file(path, options).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-release-install-{name}"))
    }
}
