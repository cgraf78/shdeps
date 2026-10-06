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

pub(crate) const ARCHIVE_LAYOUT_FILE: &str = ".shdeps-release-layout";
const ARCHIVE_BACKUP_EXTENSION: &str = "shdeps-archive-backup-";
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
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

/// Why `shdeps update` would refuse to upgrade a `github:release` root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpgradeBlocker {
    /// The stable root is a symlink. Ownership checks never follow it, so it
    /// cannot be proven to be a Shdeps archive (e.g. another installer's
    /// `current` link).
    SymlinkedRoot {
        /// Raw link target, for the report.
        target: PathBuf,
    },
    /// Something other than a directory occupies the stable root.
    NonDirectoryRoot,
    /// A real directory without the archive marker or legacy proof.
    UnprovenRoot {
        /// The legacy evidence is contradictory (`ArchiveState::Ambiguous`),
        /// which blocks raw and archive releases alike.
        ambiguous: bool,
    },
    /// The root is gone while the public command is still a link, the shape
    /// only an archive install leaves; an archive upgrade cannot prove the
    /// link is not someone else's and refuses to replace it.
    MissingRoot,
    /// The marker exists but is not a regular file with the known content;
    /// every update fails closed on it.
    InvalidMarker(String),
    /// The standalone release installer's publication lock exists (an
    /// `install.sh` is running, or one was killed and left it), so the update
    /// refuses to interpret the installer's tree. Only Unix hosts run the
    /// installer.
    #[cfg(unix)]
    InstallerLocked {
        /// The lock path to remove once no installer runs.
        lock: PathBuf,
    },
}

/// Read-only twin of the release update's layout gate.
///
/// For a dependency whose manifest row is already `github:release`,
/// `update_release::install_request` refuses a download as a "release asset
/// format changed" migration when, from the same `archive_state` snapshot, a
/// raw asset meets any state other than `None`, or an archive asset meets a
/// root that is not `Proven` while the public command path exists. Without
/// network the asset kind is unknown, so this assumes the next asset keeps
/// the root's current layout: a proven or installer-owned archive root and a
/// rootless regular public file (a raw release) are healthy, and every other
/// root that blocks either kind is reported. (A symlinked or
/// unmarked root beside a regular launcher still blocks archive upgrades and
/// is reported; that is the legacy-launcher shape of a standalone install.)
/// Before the first recorded release only the explicit marker is read, and
/// only a corrupt one blocks. It writes nothing (no marker backfill) and
/// takes no lock, so diagnostics may run beside an update; a root a
/// concurrent update is swapping can classify either way for that instant.
///
/// A root the cgraf78/actions standalone installer provably owns
/// (`standalone_layout::classify`) is adopted by the archive switch, so it is
/// not blocked, including an adoption interrupted with the root link parked;
/// while that installer's lock exists the update refuses, so that is
/// reported. Like the update, the layout is consulted only for a root that
/// is not already `Proven`, so a lock left beside an adopted root is inert. The installer only publishes archives, so a raw asset over its
/// root is an unknowable format change like any other.
///
/// The predicate is restated here rather than shared with `install_request`;
/// the CLI parity test `health_agrees_with_update_on_every_release_root`
/// runs both against each root shape and pins agreement.
pub(crate) fn upgrade_blocker(
    state_dir: &Path,
    install_base: &Path,
    public: &Path,
    name: &str,
    prior_release: bool,
) -> Result<Option<UpgradeBlocker>> {
    // Validate the marker on its own first so a corrupt marker is reported
    // as such, while any other read failure (a corrupt link ledger) stays an
    // error for the caller instead of masquerading as a marker problem.
    if let Some(install_dir) = managed_install_dir(install_base, name)? {
        match marker_state(&install_dir) {
            Ok(_) => {}
            Err(crate::Error::Io(error)) if error.kind() == io::ErrorKind::InvalidData => {
                return Ok(Some(UpgradeBlocker::InvalidMarker(error.to_string())));
            }
            Err(error) => return Err(error),
        }
    }
    // The same snapshot the update gate starts from: full legacy proof once a
    // release is recorded, otherwise only the explicit marker.
    let archive = if prior_release {
        archive_state(state_dir, install_base, public, name)?
    } else {
        explicit_archive_state(install_base, name)?
    };
    // Like the update, consult the installer layout only for a root Shdeps
    // has not proven: a marked root is its own, so a stale installer lock
    // left in the inert control directory beside it blocks nothing.
    if archive != ArchiveState::Proven {
        match crate::standalone_layout::classify(install_base, name, public) {
            crate::standalone_layout::Standalone::Adoptable => return Ok(None),
            #[cfg(unix)]
            crate::standalone_layout::Standalone::Locked(lock) => {
                return Ok(Some(UpgradeBlocker::InstallerLocked { lock }));
            }
            crate::standalone_layout::Standalone::None => {}
        }
    }
    if !prior_release {
        // Before the first recorded release install the update consults only
        // the explicit marker, which was just found valid or absent.
        return Ok(None);
    }
    match archive {
        ArchiveState::Proven => Ok(None),
        ArchiveState::Ambiguous => Ok(Some(UpgradeBlocker::UnprovenRoot { ambiguous: true })),
        ArchiveState::None => {
            let public_type = match fs::symlink_metadata(public) {
                Ok(metadata) => metadata.file_type(),
                // The archive switch replaces an unowned root only when no
                // public command could be stranded.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let root = install_base.join(name);
            let root_type = match fs::symlink_metadata(&root) {
                Ok(metadata) => metadata.file_type(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(public_type
                        .is_symlink()
                        .then_some(UpgradeBlocker::MissingRoot));
                }
                Err(error) => return Err(error.into()),
            };
            Ok(Some(if root_type.is_symlink() {
                UpgradeBlocker::SymlinkedRoot {
                    target: fs::read_link(&root).unwrap_or_default(),
                }
            } else if root_type.is_dir() {
                UpgradeBlocker::UnprovenRoot { ambiguous: false }
            } else {
                UpgradeBlocker::NonDirectoryRoot
            }))
        }
    }
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

/// Verifies the installed release command instead of a PATH shadow.
///
/// Raw installs own a regular public binary. Archive commands may be symlinks,
/// but retaining one requires it to still resolve into this dependency's root;
/// a healthy executable reached through a repointed link is not the old tool.
pub(crate) fn usable_version(
    install_base: &Path,
    public: &Path,
    name: &str,
    runner: &impl process::Runner,
) -> Option<String> {
    let metadata = fs::symlink_metadata(public).ok()?;
    if metadata.file_type().is_symlink() && !symlink_points_into(public, &install_base.join(name)) {
        return None;
    }
    process::verified_version(runner, public)
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

/// Installs a bzip2-compressed standalone release binary to an exact path.
pub(crate) fn install_bz2_to(target: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut decoder = bzip2::read::BzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;

    // Like `.gz`, Bash treats `.bz2` assets as compressed single binaries.
    // Keep the decompression-only difference isolated so raw release ownership
    // behavior has one implementation in `install_plain_to`.
    install_plain_to(target, &decoded)
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
    // bin ownership in `install_plain_to` so adding formats does not accidentally
    // drift from the raw release replacement contract.
    install_plain_to(target, &decoded)
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

    // See `switch_root` for the backup/exchange/rollback contract.
    let parked = switch_root(&content_root, &install_dir, public, &relative_binary)?;
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
        if let Some(parked) = &parked {
            if let Err(rollback) = fs::rename(parked, &install_dir) {
                return Err(io::Error::other(format!(
                    "archive public link failed and root restore also failed: link={link}, restore={rollback}"
                ))
                .into());
            }
        }
        return Err(link);
    }
    if let Some(parked) = &parked {
        // The root and its public command are now committed. Backup cleanup is
        // best-effort so an antivirus or transient handle does not turn a good
        // install into a manifest-less failure. `remove_any` uses
        // `symlink_metadata`, so a parked symlink is unlinked, never followed.
        let _ = remove_any(parked);
    }
    // A fallback switch killed after its second rename leaves the parked root
    // link beside a committed root. Shdeps alone creates that fixed name, so
    // a later successful switch retires it.
    let stale_parked = parked_root_link(&install_dir);
    if fs::symlink_metadata(&stale_parked).is_ok_and(|metadata| metadata.is_symlink()) {
        let _ = fs::remove_file(&stale_parked);
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

/// Moves the staged root into place and returns where the previous root entry
/// was parked, if there was one, so the caller can roll back or discard it.
///
/// Backup/switch/rollback pattern (mirrors `release_activate::activate`): an
/// earlier `remove_any(&install_dir)` immediately followed by
/// `rename(&content_root, &install_dir)` had a window where, if the rename
/// failed for any reason (transient FS error, permissions, a stale file handle
/// preventing the parent's directory entry from being claimed), the existing
/// install was already gone and the public symlink left pointing at a
/// now-missing path. The old install is therefore renamed to a sibling backup
/// first and restored if the switch fails. Both renames stay on the same
/// filesystem (sibling paths) so each is atomic on POSIX.
///
/// A symlinked root (the cgraf78/actions standalone installer's layout, see
/// `standalone_layout`) gets stronger treatment because commands outside
/// Shdeps' ledger resolve through it: a client launcher, Termux's
/// `$PREFIX/bin` link, or the installer's manpage link. Where the filesystem
/// can exchange two paths atomically, the staged directory and the link trade
/// places in one step, so the root never disappears. Otherwise the public
/// command is first pointed straight at the binary it currently runs and the
/// link is parked under a fixed name ([`parked_root_link`]), so a crash
/// between the two renames is recognizable and the next update finishes the
/// switch and removes the parked link. A regular launcher (dot's
/// `client-launcher.sh`) cannot be re-pointed, so on that fallback path a
/// crash exactly between the renames leaves it failing until Shdeps runs
/// again; the exchange path has no such window.
fn switch_root(
    content_root: &Path,
    install_dir: &Path,
    public: &Path,
    relative_binary: &Path,
) -> Result<Option<PathBuf>> {
    // Do not use `Path::exists()` here: it follows symlinks, so a dangling
    // repo-install root would look absent even though its directory entry still
    // blocks the destination rename. The archive switch owns entries at this
    // boundary, not whatever a prior symlink happened to target.
    let had_existing = path_entry_exists(install_dir)?;
    let parked_link = parked_root_link(install_dir);
    let mut backup = None;
    if had_existing {
        let parked = if fs::symlink_metadata(install_dir)?.file_type().is_symlink() {
            if exchange_paths(content_root, install_dir).is_ok() {
                // The old link now sits at the staged path. It is the parked
                // entry: rollback renames it back, success unlinks it, and the
                // extraction scope guard never follows it.
                return Ok(Some(content_root.to_path_buf()));
            }
            keep_command_resolvable(public, install_dir, relative_binary)?;
            parked_link
        } else {
            install_backup_path(install_dir)
        };
        fs::rename(install_dir, &parked)?;
        backup = Some(parked);
    } else if fs::symlink_metadata(&parked_link).is_ok_and(|metadata| metadata.is_symlink()) {
        // A previous fallback switch parked the root link and was killed
        // before the new root moved in. Treat that link as the prior root:
        // rollback restores it, success removes it.
        backup = Some(parked_link);
    }
    if let Err(switch) = fs::rename(content_root, install_dir) {
        if let Some(parked) = backup.as_ref().filter(|_| had_existing) {
            if let Err(rollback) = fs::rename(parked, install_dir) {
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
        let _ = remove_any(content_root);
        return Err(switch.into());
    }
    Ok(backup)
}

/// Fixed sibling name for a symlinked root parked by the fallback switch.
///
/// Unlike the unique backup names used for real directories, this one is
/// deterministic so an interrupted switch is recognizable afterwards (see
/// `standalone_layout::classify`). The name is reserved: Shdeps treats a
/// symlink there as its own parked root and replaces or retires it. The
/// `.shdeps-` infix keeps it out of any upstream or installer namespace.
pub(crate) fn parked_root_link(install_dir: &Path) -> PathBuf {
    let mut name = install_dir
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".shdeps-parked-root");
    install_dir.with_file_name(name)
}

/// Atomically swaps two existing paths on the same filesystem, or fails without
/// changing either.
///
/// Callers must treat every error as "not swapped" and fall back: kernels
/// before 3.15 report ENOSYS, and filesystems without exchange support (WSL1
/// drvfs, NFS, older ZFS, overlay stacks, HFS+) report EINVAL, EOPNOTSUPP, or
/// EXDEV. A failed `renameat2`/`renamex_np` never changes either path.
///
/// Android never attempts the syscall: the crate-wide
/// [`crate::repo_transition::renameat2_allowed`] policy (an older app
/// seccomp filter may SIGSYS-kill the process) makes this report
/// `Unsupported`, so Termux always takes the park-and-resume path. Non-Unix
/// platforms have no exchange primitive.
fn exchange_paths(left: &Path, right: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        if let Some(errno) = EXCHANGE_FAULT.with(std::cell::Cell::get) {
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
    #[cfg(unix)]
    {
        if !crate::repo_transition::renameat2_allowed() {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        crate::repo_transition::rename_exchange(left, right)
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
thread_local! {
    /// Errno a test injects in place of the exchange syscall's result.
    static EXCHANGE_FAULT: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// Runs `body` with every exchange on this thread failing with `errno`, the
/// way a kernel or filesystem without support would (tests only).
#[cfg(test)]
pub(crate) fn with_exchange_errno<T>(errno: i32, body: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            EXCHANGE_FAULT.with(|fault| fault.set(None));
        }
    }
    EXCHANGE_FAULT.with(|fault| fault.set(Some(errno)));
    let _restore = Restore;
    body()
}

/// Runs `body` as if this platform could not exchange paths (tests only).
#[cfg(test)]
pub(crate) fn without_exchange<T>(body: impl FnOnce() -> T) -> T {
    with_exchange_errno(libc::ENOSYS, body)
}

/// Before a symlinked root is renamed away, points a public command symlink
/// that currently resolves through the root straight at the same physical
/// binary, creating it when absent. A regular launcher or a link elsewhere is
/// never touched. The switch's final link publication then re-points the
/// command into the new root.
#[cfg(unix)]
fn keep_command_resolvable(
    public: &Path,
    install_dir: &Path,
    relative_binary: &Path,
) -> Result<()> {
    let Ok(root) = fs::canonicalize(install_dir) else {
        // A dangling root link resolves nothing, so nothing can break.
        return Ok(());
    };
    let target = match fs::symlink_metadata(public) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(public).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Commands published outside SHDEPS_BIN_DIR (Termux's installer
            // uses `$PREFIX/bin`) cannot be re-pointed; publishing Shdeps' own
            // command early still leaves one that survives the switch.
            fs::canonicalize(install_dir.join(relative_binary)).ok()
        }
        _ => None,
    };
    let Some(target) = target.filter(|target| target.starts_with(&root) && target.is_file()) else {
        return Ok(());
    };
    replace_symlink(&target, public).map(|_| ())
}

#[cfg(not(unix))]
fn keep_command_resolvable(
    _public: &Path,
    _install_dir: &Path,
    _relative_binary: &Path,
) -> Result<()> {
    Ok(())
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
        "{ARCHIVE_BACKUP_EXTENSION}{}-{nanos}",
        std::process::id()
    ))
}

/// Lists backups that an interrupted archive swap left beside a release root.
///
/// The swap renames the live root to [`install_backup_path`] and removes it
/// after the new root is in place, so a survivor means a crash in between
/// (or a swap still deleting the old tree). Matching uses the same
/// `with_extension` derivation, so a repo name that already contains a dot is
/// found too; that derivation also means `owner/foo` and `owner/foo.nvim`
/// share backup names, so such a backup is listed for both. One `read_dir` of
/// the owner directory; an unreadable directory reports nothing.
pub(crate) fn archive_backups(install_base: &Path, name: &str) -> Vec<PathBuf> {
    let prefix_path = install_base
        .join(name)
        .with_extension(ARCHIVE_BACKUP_EXTENSION);
    let (Some(parent), Some(prefix)) = (
        prefix_path.parent(),
        prefix_path.file_name().and_then(|prefix| prefix.to_str()),
    ) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut backups = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|file| file.starts_with(prefix) && file.len() > prefix.len())
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    backups.sort();
    backups
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

        let path = super::install_plain_to(&dir.join("bin/tool"), b"binary").unwrap();

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

        super::install_plain_to(&dir.join("bin/tool"), b"release").unwrap();

        assert_eq!(fs::read(target).unwrap(), b"release");
    }

    #[test]
    #[cfg(unix)]
    fn gz_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("gz-single");
        let bytes = gzip(b"binary");

        let target = super::install_gz_to(&dir.join("tool"), &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn bz2_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("bz2-single");
        let bytes = bzip2(b"binary");

        let target = super::install_bz2_to(&dir.join("tool"), &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn xz_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("xz-single");
        let bytes = xz(b"binary");

        let target = super::install_xz_to(&dir.join("tool"), &bytes).unwrap();

        assert_eq!(target, dir.join("tool"));
        assert_eq!(fs::read(&target).unwrap(), b"binary");
        assert!(fs::metadata(&target).unwrap().permissions().mode() & 0o111 != 0);
    }

    #[test]
    #[cfg(unix)]
    fn zst_install_decompresses_single_binary_and_marks_executable() {
        let dir = temp_dir("zst-single");
        let bytes = zstd(b"binary");

        let target = super::install_zst_to(&dir.join("tool"), &bytes).unwrap();

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

        let public = super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();
        assert!(dir.join("bin/old-helper").is_symlink());

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
            "owner/tool",
            "tool",
            &bytes_v1,
        )
        .unwrap();
        assert!(dir.join("bin/tool").is_symlink());
        assert!(dir.join("bin/tool-helper").is_symlink());

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_tar_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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
    fn archive_install_restores_a_symlinked_root_when_public_link_fails() {
        // Covers both switch flavors: after an exchange the old link is parked
        // at the staged path, after the fallback under the fixed parked name.
        // Either way a failed publication must restore the exact old link and
        // leave no staged, parked, or backup entries behind.
        for exchange in [true, false] {
            let dir = temp_dir(&format!("archive-symlinked-root-rollback-{exchange}"));
            let install_base = dir.join("share");
            let old_release = install_base.join("owner/.tool-standalone/releases/old");
            let install_dir = install_base.join("owner/tool");
            fs::create_dir_all(&old_release).unwrap();
            crate::test_support::write_executable(&old_release.join("tool"), "#!/bin/sh\n");
            symlink(".tool-standalone/releases/old", &install_dir).unwrap();
            let blocker = dir.join("blocked");
            fs::write(&blocker, "sentinel").unwrap();

            let install = || {
                super::install_tar_gz_to(
                    &dir.join("state"),
                    &install_base,
                    &blocker.join("tool"),
                    "owner/tool",
                    "tool",
                    &tar_gz(&[("tool", b"v2".as_slice(), 0o755)]),
                )
            };
            let error = if exchange {
                install().unwrap_err()
            } else {
                super::without_exchange(install).unwrap_err()
            };

            assert!(!error.to_string().is_empty());
            assert_eq!(
                fs::read_link(&install_dir).unwrap(),
                std::path::Path::new(".tool-standalone/releases/old")
            );
            let mut leftovers: Vec<_> = fs::read_dir(install_base.join("owner"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            leftovers.sort();
            assert_eq!(
                leftovers,
                [".tool-standalone", "tool"],
                "exchange={exchange}"
            );
        }
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

    /// Builds a staged root plus a symlinked live root (the standalone
    /// installer's shape) and returns `(staged, root, old_binary, public)`.
    #[cfg(unix)]
    fn symlinked_root_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let old_release = dir.join("share/owner/.tool-standalone/releases/old");
        let staged = dir.join("share/owner/.tool.tmp.1");
        let root = dir.join("share/owner/tool");
        let public = dir.join("bin/tool");
        fs::create_dir_all(&old_release).unwrap();
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        crate::test_support::write_executable(&old_release.join("tool"), "#!/bin/sh\necho old\n");
        crate::test_support::write_executable(&staged.join("tool"), "#!/bin/sh\necho new\n");
        symlink(".tool-standalone/releases/old", &root).unwrap();
        symlink(root.join("tool"), &public).unwrap();
        let old_binary = fs::canonicalize(old_release.join("tool")).unwrap();
        (staged, root, old_binary, public)
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn symlinked_root_switch_exchanges_without_a_missing_root() {
        let (staged, root, old_binary, public) = symlinked_root_fixture("switch-exchange");

        let parked =
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap();

        // Whether the exchange path ran depends on the test filesystem, not on
        // the code: tmpfs/ext4/btrfs/APFS exchange, while NFS, WSL1 drvfs, or
        // older ZFS decline. Assert whichever switch this filesystem selects.
        if exchange_supported(root.parent().unwrap()) {
            assert_eq!(parked.as_deref(), Some(staged.as_path()));
            assert_eq!(
                fs::read_link(&staged).unwrap(),
                std::path::Path::new(".tool-standalone/releases/old")
            );
            assert_eq!(fs::read_link(&public).unwrap(), root.join("tool"));
        } else {
            assert_eq!(parked, Some(super::parked_root_link(&root)));
            assert_eq!(fs::read_link(&public).unwrap(), old_binary);
        }
        assert!(
            !fs::symlink_metadata(&root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(root.join("tool")).unwrap(),
            "#!/bin/sh\necho new\n"
        );
        assert!(old_binary.is_file(), "the old release is never touched");
    }

    /// Probes whether this build and filesystem can exchange a directory with
    /// a symlink, the exact shape `switch_root` swaps.
    #[cfg(unix)]
    fn exchange_supported(dir: &std::path::Path) -> bool {
        let probe_dir = dir.join(".exchange-probe-dir");
        let probe_link = dir.join(".exchange-probe-link");
        fs::create_dir_all(&probe_dir).unwrap();
        let _ = fs::remove_file(&probe_link);
        symlink("target", &probe_link).unwrap();
        let supported = super::exchange_paths(&probe_dir, &probe_link).is_ok();
        let _ = fs::remove_file(&probe_dir);
        let _ = fs::remove_dir(&probe_dir);
        let _ = fs::remove_file(&probe_link);
        let _ = fs::remove_dir(&probe_link);
        supported
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_root_switch_falls_back_for_every_unsupported_exchange_errno() {
        // Kernels without renameat2 (ENOSYS) and filesystems without exchange
        // support (EINVAL on NFS/WSL1 drvfs, EOPNOTSUPP on older ZFS, EXDEV on
        // overlay stacks) must select the park-and-resume switch, never fail.
        for errno in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP, libc::EXDEV] {
            let (staged, root, old_binary, public) =
                symlinked_root_fixture(&format!("switch-errno-{errno}"));

            let parked = super::with_exchange_errno(errno, || {
                super::switch_root(&staged, &root, &public, std::path::Path::new("tool"))
            })
            .unwrap_or_else(|error| panic!("errno {errno} must fall back: {error}"));

            assert_eq!(
                parked,
                Some(super::parked_root_link(&root)),
                "errno {errno}"
            );
            assert_eq!(fs::read_link(&public).unwrap(), old_binary, "errno {errno}");
            assert!(
                !fs::symlink_metadata(&root)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "errno {errno}"
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn symlinked_root_switch_parks_under_the_android_rename_policy() {
        // The crate-wide rename policy, simulated on any host, must steer the
        // standalone switch to park-and-resume even where the filesystem
        // could exchange.
        let (staged, root, old_binary, public) = symlinked_root_fixture("switch-android-policy");

        let parked = crate::repo_transition::with_android_rename_policy(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool"))
        })
        .unwrap();

        assert_eq!(parked, Some(super::parked_root_link(&root)));
        assert_eq!(fs::read_link(&public).unwrap(), old_binary);
    }

    #[test]
    #[cfg(target_os = "android")]
    fn symlinked_root_switch_never_attempts_exchange_on_android() {
        // Termux CI: the exchange syscall may be fatal under older app seccomp
        // policies, so the build must select the fallback without trying it.
        assert!(!crate::repo_transition::RENAMEAT2_ALLOWED);
        let (staged, root, old_binary, public) = symlinked_root_fixture("switch-android");

        let parked =
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap();

        assert_eq!(parked, Some(super::parked_root_link(&root)));
        assert_eq!(fs::read_link(&public).unwrap(), old_binary);
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_root_fallback_switch_pins_the_public_command_first() {
        let (staged, root, old_binary, public) = symlinked_root_fixture("switch-fallback");

        let parked = super::without_exchange(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap()
        })
        .unwrap();

        // Before the root moved, the command was pointed straight at the
        // binary it ran, so a crash between the renames left it working, and
        // the link was parked under the fixed, recognizable name.
        assert_eq!(fs::read_link(&public).unwrap(), old_binary);
        assert_eq!(parked, super::parked_root_link(&root));
        assert_eq!(
            fs::read_link(&parked).unwrap(),
            std::path::Path::new(".tool-standalone/releases/old")
        );
        assert!(
            !fs::symlink_metadata(&root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_root_fallback_switch_publishes_a_missing_command_early() {
        let (staged, root, old_binary, public) = symlinked_root_fixture("switch-fallback-absent");
        fs::remove_file(&public).unwrap();

        super::without_exchange(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap()
        });

        assert_eq!(fs::read_link(&public).unwrap(), old_binary);
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_root_fallback_switch_never_touches_launchers_or_foreign_links() {
        let (staged, root, _old_binary, public) =
            symlinked_root_fixture("switch-fallback-launcher");
        fs::remove_file(&public).unwrap();
        crate::test_support::write_executable(&public, "#!/bin/sh\nexec launcher\n");

        super::without_exchange(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap()
        });
        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "#!/bin/sh\nexec launcher\n"
        );

        let (staged, root, _old_binary, public) = symlinked_root_fixture("switch-fallback-foreign");
        let foreign = public.with_file_name("elsewhere");
        crate::test_support::write_executable(&foreign, "#!/bin/sh\n");
        fs::remove_file(&public).unwrap();
        symlink(&foreign, &public).unwrap();

        super::without_exchange(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap()
        });
        assert_eq!(fs::read_link(&public).unwrap(), foreign);

        // A non-canonical spelling of a foreign target would be rewritten to
        // its canonical form if the "resolves into the root" filter were lost,
        // so this also pins that filter, not just the final target.
        let (staged, root, _old_binary, public) =
            symlinked_root_fixture("switch-fallback-foreign-noncanonical");
        let foreign = public.with_file_name("elsewhere");
        crate::test_support::write_executable(&foreign, "#!/bin/sh\n");
        let spelled = public.parent().unwrap().join("../bin/elsewhere");
        fs::remove_file(&public).unwrap();
        symlink(&spelled, &public).unwrap();

        super::without_exchange(|| {
            super::switch_root(&staged, &root, &public, std::path::Path::new("tool")).unwrap()
        });
        assert_eq!(fs::read_link(&public).unwrap(), spelled);
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

        let public = super::install_tar_gz_to(
            &state_as_file,
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let error = super::install_tar_gz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_tar_bz2_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_tar_zst_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_tar_xz_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_zip_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_zip_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

        let public = super::install_zip_to(
            &dir.join("state"),
            &dir.join("share"),
            &dir.join("bin/tool"),
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

    /// Layout fixture for `upgrade_blocker`: (state, install base, public).
    fn blocker_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let install_base = dir.join("share");
        fs::create_dir_all(install_base.join("owner")).unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        (dir.join("state"), install_base, dir.join("bin/tool"))
    }

    fn blocker(
        state: &std::path::Path,
        base: &std::path::Path,
        public: &std::path::Path,
        prior_release: bool,
    ) -> Option<super::UpgradeBlocker> {
        super::upgrade_blocker(state, base, public, "owner/tool", prior_release).unwrap()
    }

    #[test]
    fn upgrade_blocker_allows_absent_marked_and_legacy_proven_roots() {
        let (state, base, public) = blocker_fixture("blocker-allowed");
        fs::write(&public, "raw").unwrap();
        assert_eq!(blocker(&state, &base, &public, true), None, "raw release");

        fs::remove_file(&public).unwrap();
        let root = base.join("owner/tool");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/tool"), "bin").unwrap();
        symlink(root.join("bin/tool"), &public).unwrap();
        assert_eq!(
            blocker(&state, &base, &public, true),
            None,
            "a live public link into the root proves a pre-marker archive"
        );

        fs::write(root.join(super::ARCHIVE_LAYOUT_FILE), "v1 archive\n").unwrap();
        assert_eq!(blocker(&state, &base, &public, true), None, "marked root");
    }

    #[test]
    fn upgrade_blocker_reports_symlinked_root_only_when_update_would_refuse() {
        let (state, base, public) = blocker_fixture("blocker-symlink");
        fs::create_dir_all(base.join("owner/.tool-standalone/current")).unwrap();
        symlink(".tool-standalone/current", base.join("owner/tool")).unwrap();

        // Without a public command or a prior release the archive switch
        // replaces the unowned link, so nothing blocks.
        assert_eq!(blocker(&state, &base, &public, true), None);
        symlink(base.join("owner/tool/tool"), &public).unwrap();
        assert_eq!(blocker(&state, &base, &public, false), None);

        assert_eq!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::SymlinkedRoot {
                target: PathBuf::from(".tool-standalone/current")
            })
        );
    }

    #[test]
    fn upgrade_blocker_reports_unproven_and_ambiguous_directories() {
        let (state, base, public) = blocker_fixture("blocker-unproven");
        fs::create_dir_all(base.join("owner/tool")).unwrap();
        let elsewhere = public.with_file_name("elsewhere");
        fs::write(&elsewhere, "bin").unwrap();
        symlink(&elsewhere, &public).unwrap();
        assert_eq!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::UnprovenRoot { ambiguous: false })
        );

        fs::remove_file(&public).unwrap();
        fs::write(&public, "launcher").unwrap();
        assert_eq!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::UnprovenRoot { ambiguous: true })
        );
    }

    #[test]
    fn upgrade_blocker_reports_non_directory_root_and_corrupt_marker() {
        let (state, base, public) = blocker_fixture("blocker-invalid");
        fs::write(base.join("owner/tool"), "file").unwrap();
        fs::write(&public, "bin").unwrap();
        assert_eq!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::NonDirectoryRoot)
        );

        fs::remove_file(base.join("owner/tool")).unwrap();
        fs::create_dir_all(base.join("owner/tool")).unwrap();
        fs::write(
            base.join("owner/tool").join(super::ARCHIVE_LAYOUT_FILE),
            "v2\n",
        )
        .unwrap();
        assert!(matches!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::InvalidMarker(reason)) if reason.contains("unknown release archive marker")
        ));
    }

    #[test]
    fn upgrade_blocker_reports_missing_root_behind_a_public_link() {
        let (state, base, public) = blocker_fixture("blocker-missing-root");
        symlink(base.join("owner/tool/bin/tool"), &public).unwrap();

        assert_eq!(
            blocker(&state, &base, &public, true),
            Some(super::UpgradeBlocker::MissingRoot)
        );
        assert_eq!(
            blocker(&state, &base, &public, false),
            None,
            "before the first release install only the marker is consulted"
        );
    }

    #[test]
    fn upgrade_blocker_reads_only_a_corrupt_marker_before_the_first_release() {
        let (state, base, public) = blocker_fixture("blocker-first-release");
        fs::create_dir_all(base.join("owner/tool")).unwrap();
        fs::write(&public, "bin").unwrap();
        assert_eq!(blocker(&state, &base, &public, false), None);

        fs::write(
            base.join("owner/tool").join(super::ARCHIVE_LAYOUT_FILE),
            "v2\n",
        )
        .unwrap();
        assert!(matches!(
            blocker(&state, &base, &public, false),
            Some(super::UpgradeBlocker::InvalidMarker(_))
        ));
    }

    #[test]
    fn upgrade_blocker_never_writes_the_marker() {
        let (state, base, public) = blocker_fixture("blocker-read-only");
        let root = base.join("owner/tool");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/tool"), "bin").unwrap();
        symlink(root.join("bin/tool"), &public).unwrap();

        assert_eq!(blocker(&state, &base, &public, true), None);
        assert!(!root.join(super::ARCHIVE_LAYOUT_FILE).exists());
        assert!(!state.exists());
    }

    #[test]
    fn archive_backups_match_only_this_roots_swap_leftovers() {
        let dir = temp_dir("archive-backups");
        let base = dir.join("share");
        for name in [
            "owner/tool.shdeps-archive-backup-1-2",
            "owner/tool.shdeps-archive-backup-3-4",
            "owner/toolkit.shdeps-archive-backup-1-2",
            "owner/tool",
            "owner/foo.shdeps-archive-backup-5-6",
        ] {
            fs::create_dir_all(base.join(name)).unwrap();
        }

        assert_eq!(
            super::archive_backups(&base, "owner/tool"),
            [
                base.join("owner/tool.shdeps-archive-backup-1-2"),
                base.join("owner/tool.shdeps-archive-backup-3-4"),
            ]
        );
        // `with_extension` replaces a dotted repo suffix, exactly as the swap
        // names its backup, so `foo.nvim` backs up to `foo.shdeps-…`.
        assert_eq!(
            super::archive_backups(&base, "owner/foo.nvim"),
            [base.join("owner/foo.shdeps-archive-backup-5-6")]
        );
        assert!(super::archive_backups(&base, "missing/tool").is_empty());
    }
}
