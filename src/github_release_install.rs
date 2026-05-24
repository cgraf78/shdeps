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

use crate::archive;
use crate::extras;
use crate::process;
use crate::Result;

/// Installs a raw standalone release binary into `SHDEPS_BIN_DIR`.
pub fn install_plain(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let target = bin_dir.join(cmd);
    let tmp = temp_path(&target);

    fs::write(&tmp, bytes)?;
    make_executable(&tmp)?;

    // `github:release` is the historical exception to shdeps' normal "never
    // overwrite regular files in SHDEPS_BIN_DIR" rule. Bash downloads/moves the
    // selected asset directly to the requested bin path, so preserve that
    // replacement behavior here and keep it isolated from the safer symlink
    // helpers used by repo, cargo, go, uv, and npm installs.
    fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Installs a gzip-compressed standalone release binary.
pub fn install_gz(bin_dir: &Path, cmd: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;

    // Bash treats `.gz` release assets as compressed singles, not archives.
    // Reuse the plain-binary path after decompression so replacement and
    // executable-bit behavior stay identical to uncompressed release assets.
    install_plain(bin_dir, cmd, &decoded)
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
    install_archive(state_dir, install_base, bin_dir, name, cmd, |dest| {
        archive::unpack_tar_gz(bytes, dest).map(|_| ())
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
    install_archive(state_dir, install_base, bin_dir, name, cmd, |dest| {
        archive::unpack_zip(bytes, dest).map(|_| ())
    })
}

fn install_archive(
    state_dir: &Path,
    install_base: &Path,
    bin_dir: &Path,
    name: &str,
    cmd: &str,
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
    let binary = find_binary(&content_root, cmd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{cmd} binary not found in release archive"),
        )
    })?;
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
    remove_any(&install_dir)?;
    fs::rename(&content_root, &install_dir)?;
    if content_root != extract_dir {
        remove_any(&extract_dir)?;
    }

    let source = install_dir.join(relative_binary);
    let public = bin_dir.join(cmd);
    replace_symlink(&source, &public)?;
    // Release archives commonly carry completions or man pages beside the
    // binary. Reusing the shared extras linker keeps those secondary artifacts
    // tracked and prunable exactly like repo-based installs.
    extras::link(state_dir, install_base, name, &install_dir)?;
    Ok(public)
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

fn find_binary(root: &Path, cmd: &str) -> Option<PathBuf> {
    let mut prefixed = None;
    for path in walk_files(root) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !process::executable_path(&path) {
            continue;
        }
        if name == cmd {
            return Some(path);
        }
        // Some projects ship platform-suffixed binaries inside a generic
        // archive. Prefer the exact command when present, but keep the first
        // executable `cmd-*`/`cmd_*` fallback to preserve the useful part of
        // Bash's looser archive probing without guessing unrelated filenames.
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

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::remove_file(target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    symlink(source, target)?;
    Ok(())
}

#[cfg(not(unix))]
fn replace_symlink(source: &Path, target: &Path) -> Result<()> {
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

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, Header};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

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
        assert!(fs::read_dir(dir.join("share/owner"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")));
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

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
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
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
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
