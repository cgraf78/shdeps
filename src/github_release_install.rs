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
use crate::extras;
use crate::process;

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
    let had_existing = install_dir.exists();
    if had_existing {
        fs::rename(&install_dir, &backup)?;
    }
    if let Err(switch) = fs::rename(&content_root, &install_dir) {
        if had_existing {
            if let Err(rollback) = fs::rename(&backup, &install_dir) {
                // Two failures in a row: the live-switch and the
                // restore both failed. Leave both `content_root` and
                // `backup` on disk so the user can inspect what's
                // there rather than silently discarding evidence; the
                // returned error carries the switch error (the
                // primary cause), with the rollback error appended.
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
    if had_existing {
        // Live install successfully switched. Backup cleanup is
        // best-effort so a transient file handle or an antivirus
        // scanner does not turn a successful install into a rollback
        // of a good install. Use `remove_any` (not
        // `fs::remove_dir_all`) so that if `install_dir` happened to
        // be a symlink to a real directory — unusual but legal — the
        // backup we just renamed is a symlink whose target must not
        // be touched. `remove_any` routes through `symlink_metadata`
        // and unlinks the symlink entry without following it.
        let _ = remove_any(&backup);
    }
    if content_root != extract_dir {
        remove_any(&extract_dir)?;
    }

    let source = install_dir.join(relative_binary);
    replace_symlink(&source, public)?;
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
    for path in walk_files(root) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == cmd {
            if process::executable_path(&path) || allow_non_executable_exact_binary {
                return Some(path);
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
    prefixed
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
fn replace_symlink(source: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if source == target {
        // Custom hooks can deliberately place the public command at the binary
        // path inside the managed install tree. Dotfiles' Neovim hook does this
        // so ~/.local/bin/nvim can remain a launcher while the real editor
        // lives at ~/.local/share/neovim/neovim/bin/nvim. In that layout there
        // is nothing to link: replacing `target` would first remove the real
        // binary, then create a self-referential symlink that can never exec.
        return Ok(());
    }

    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "release public-bin target has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    // Same staging+rename TOCTOU narrowing the consolidated
    // `extras::replace_symlink` helper applies. We can't share the
    // helper directly because `github:release` deliberately overwrites
    // regular files at `target` — a Bash-parity carve-out documented at
    // the top of `install_plain` — whereas `extras::replace_symlink`
    // preserves non-symlinks. Use the same staging pattern inline so the
    // missing-path window between `remove_file` and `symlink` is
    // eliminated for the public-bin link: the path is always either the
    // old binary/symlink or the new symlink, never absent. Without this,
    // an external concurrent writer to `~/.local/bin` could observe the
    // momentary gap (the same window `bin_link::one` was rewritten to
    // close in round 8).
    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "release public-bin target has no file name",
        )
    })?;
    let staging = parent.join(format!(
        ".{}.shdeps-release-link.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    ));
    // Remove any prior staging leftover from a crashed write before we
    // create our own; `create_dir_all(parent)` does not touch existing
    // files and `symlink` would EEXIST against a stale entry.
    let _ = fs::remove_file(&staging);
    symlink(source, &staging)?;
    if let Err(error) = fs::rename(&staging, target) {
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn replace_symlink(source: &Path, target: &Path) -> Result<()> {
    if source == target {
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, target)?;
    Ok(())
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use bzip2::Compression as BzCompression;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

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
    fn archive_install_uses_backup_swap_and_cleans_up() {
        // Regression for the iteration-3 codex finding that
        // `install_archive` used to `remove_any(install_dir)` BEFORE
        // the final rename, leaving a window where a failed rename
        // would strand the user with no install at all. The new flow
        // moves the existing install to a sibling backup, renames the
        // staged content into place, and only then removes the
        // backup. The happy-path observable is: existing install is
        // replaced AND no `.<name>.shdeps-archive-backup-*` sibling
        // remains afterward. A failure-path rollback test would
        // require simulating a rename(2) failure mid-flow, which
        // needs platform tricks beyond what we can portably do here;
        // the no-leftover assertion catches the most common
        // backup-mechanism regression (forgetting to clean up).
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
    fn archive_install_replaces_public_bin_via_atomic_rename() {
        // Regression for the iteration-3 paladin finding that this
        // file's local `replace_symlink` used a wider `remove_file` +
        // `symlink` TOCTOU window than the shared
        // `extras::replace_symlink`. The path is now staged in the
        // same parent dir and `rename`-d into place, so a successful
        // install must leave NO `.tool.shdeps-release-link.*` staging
        // file behind in the public-bin parent directory. If a future
        // edit reintroduces the older delete-then-create pattern this
        // assertion is unchanged but the missing-path window is back;
        // the better signal is the absence of any staging leftover
        // file, which the rename success path always cleans by moving
        // it onto the target.
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
            .filter(|n| n.starts_with(".tool.shdeps-release-link."))
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
        let dir = std::env::temp_dir().join(format!(
            "shdeps-release-install-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
