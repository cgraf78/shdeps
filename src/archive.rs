//! Safe extraction helpers for shdeps-owned release archives.
//!
//! The Bash reference delegates extraction to `tar`, which preserves broad
//! compatibility but does not give shdeps a chance to reject hostile archive
//! entries before they touch disk. Rust release self-update uses this module
//! for its own artifacts so traversal and link tricks fail closed while normal
//! shdeps release tarballs still extract with their executable bits intact.

use std::fs;
use std::io::{self, Cursor, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::{Archive, EntryType};
use xz2::read::XzDecoder;
use zip::ZipArchive;

/// Extracts a gzip-compressed tar archive into `dest`.
pub fn unpack_tar_gz(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    unpack_tar(GzDecoder::new(Cursor::new(bytes)), dest)
}

/// Extracts a bzip2-compressed tar archive into `dest`.
pub fn unpack_tar_bz2(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    unpack_tar(BzDecoder::new(Cursor::new(bytes)), dest)
}

/// Extracts a zstd-compressed tar archive into `dest`.
pub fn unpack_tar_zst(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    unpack_tar(zstd::stream::read::Decoder::new(Cursor::new(bytes))?, dest)
}

/// Extracts an xz-compressed tar archive into `dest`.
pub fn unpack_tar_xz(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    unpack_tar(XzDecoder::new(Cursor::new(bytes)), dest)
}

fn unpack_tar(reader: impl Read, dest: &Path) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dest)?;
    let mut archive = Archive::new(reader);
    let mut extracted = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let Some(relative) = safe_entry_path(entry.path()?.as_ref())? else {
            // Some release archives include an explicit `.` directory entry as
            // top-level metadata. GNU/BSD tar treat that as harmless context
            // for the following files, and the Bash implementation inherited
            // that behavior. Preserve compatibility by ignoring only this
            // normalized-empty path after traversal checks have run; entries
            // with real parent/root components still fail closed below.
            continue;
        };
        reject_links(entry.header().entry_type())?;
        let target = dest.join(&relative);

        // Validate before unpacking each entry instead of relying on cleanup
        // after failure. Self-update may be replacing the tool that performs
        // recovery, so a bad archive should never get a partial chance to
        // write outside the staging directory.
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
        extracted.push(relative);
    }

    Ok(extracted)
}

/// Extracts a zip archive into `dest`.
pub fn unpack_zip(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dest)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut extracted = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = safe_zip_path(&entry)?;
        reject_zip_link(&entry)?;
        let target = dest.join(&relative);

        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            extracted.push(relative);
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&target)?;
        io::copy(&mut entry, &mut output)?;
        set_zip_mode(&target, &entry)?;
        extracted.push(relative);
    }

    Ok(extracted)
}

fn safe_entry_path(path: &Path) -> io::Result<Option<PathBuf>> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe archive path {}", path.display()),
                ));
            }
        }
    }

    if safe.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(safe))
}

fn safe_zip_path(entry: &zip::read::ZipFile<'_>) -> io::Result<PathBuf> {
    // `ZipFile::enclosed_name` handles absolute paths and `..` traversal, but
    // zip archives created on Windows can carry backslashes. On Unix those are
    // legal filename bytes, which would make an archive that looks like
    // `bin\tool` fail binary discovery and could hide malicious intent. Reject
    // them uniformly so release archives use one portable path grammar.
    if entry.name().contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsafe zip path {}", entry.name()),
        ));
    }
    entry.enclosed_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsafe zip path {}", entry.name()),
        )
    })
}

fn reject_links(kind: EntryType) -> io::Result<()> {
    if kind.is_symlink() || kind.is_hard_link() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive links are not allowed in shdeps release archives",
        ));
    }
    Ok(())
}

fn reject_zip_link(entry: &zip::read::ZipFile<'_>) -> io::Result<()> {
    if let Some(mode) = entry.unix_mode() {
        if mode & 0o170000 == 0o120000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zip links are not allowed in shdeps release archives",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_zip_mode(path: &Path, entry: &zip::read::ZipFile<'_>) -> io::Result<()> {
    if let Some(mode) = entry.unix_mode() {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_zip_mode(_path: &Path, _entry: &zip::read::ZipFile<'_>) -> io::Result<()> {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use bzip2::Compression as BzCompression;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, EntryType, Header};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::{
        safe_entry_path, unpack_tar_bz2, unpack_tar_gz, unpack_tar_xz, unpack_tar_zst, unpack_zip,
    };

    #[test]
    fn unpack_tar_gz_extracts_safe_files() {
        let dest = temp_dir("safe");
        let bytes = tar_gz(&[
            Entry::file("shdeps", b"binary"),
            Entry::file("man/man1/shdeps.1", b"man"),
        ]);

        let extracted = unpack_tar_gz(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(fs::read(dest.join("man/man1/shdeps.1")).unwrap(), b"man");
        assert!(extracted.contains(&PathBuf::from("shdeps")));
        assert!(extracted.contains(&PathBuf::from("man/man1/shdeps.1")));
    }

    #[test]
    fn unpack_tar_bz2_extracts_safe_files() {
        let dest = temp_dir("bz2-safe");
        let bytes = tar_bz2(&[
            Entry::file("shdeps", b"binary"),
            Entry::file("man/man1/shdeps.1", b"man"),
        ]);

        let extracted = unpack_tar_bz2(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(fs::read(dest.join("man/man1/shdeps.1")).unwrap(), b"man");
        assert!(extracted.contains(&PathBuf::from("shdeps")));
        assert!(extracted.contains(&PathBuf::from("man/man1/shdeps.1")));
    }

    #[test]
    fn unpack_tar_zst_extracts_safe_files() {
        let dest = temp_dir("zst-safe");
        let bytes = tar_zst(&[
            Entry::file("shdeps", b"binary"),
            Entry::file("man/man1/shdeps.1", b"man"),
        ]);

        let extracted = unpack_tar_zst(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(fs::read(dest.join("man/man1/shdeps.1")).unwrap(), b"man");
        assert!(extracted.contains(&PathBuf::from("shdeps")));
        assert!(extracted.contains(&PathBuf::from("man/man1/shdeps.1")));
    }

    #[test]
    fn unpack_tar_xz_extracts_safe_files() {
        let dest = temp_dir("xz-safe");
        let bytes = tar_xz(&[
            Entry::file("shdeps", b"binary"),
            Entry::file("man/man1/shdeps.1", b"man"),
        ]);

        let extracted = unpack_tar_xz(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(fs::read(dest.join("man/man1/shdeps.1")).unwrap(), b"man");
        assert!(extracted.contains(&PathBuf::from("shdeps")));
        assert!(extracted.contains(&PathBuf::from("man/man1/shdeps.1")));
    }

    #[test]
    fn unpack_tar_gz_rejects_parent_traversal_and_absolute_paths() {
        let traversal = safe_entry_path(std::path::Path::new("../escape")).unwrap_err();
        let absolute = safe_entry_path(std::path::Path::new("/tmp/escape")).unwrap_err();

        assert_eq!(traversal.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(absolute.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn unpack_tar_gz_skips_metadata_only_current_directory_entries() {
        let dest = temp_dir("dot-entry");
        let bytes = tar_gz(&[Entry::directory("."), Entry::file("shdeps", b"binary")]);

        let extracted = unpack_tar_gz(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(extracted, vec![PathBuf::from("shdeps")]);
    }

    #[test]
    fn unpack_tar_gz_rejects_symlinks_and_hardlinks() {
        let dest = temp_dir("links");

        let symlink = unpack_tar_gz(
            &tar_gz(&[Entry::link("shdeps-link", "shdeps", true)]),
            &dest,
        )
        .unwrap_err();
        let hardlink = unpack_tar_gz(
            &tar_gz(&[Entry::link("shdeps-hard", "shdeps", false)]),
            &dest,
        )
        .unwrap_err();

        assert_eq!(symlink.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(hardlink.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(unix)]
    fn unpack_zip_extracts_safe_files_and_permissions() {
        let dest = temp_dir("zip-safe");
        let bytes = zip(&[
            ZipEntry::file("shdeps", b"binary", 0o755),
            ZipEntry::file("man/man1/shdeps.1", b"man", 0o644),
        ]);

        let extracted = unpack_zip(&bytes, &dest).unwrap();

        assert_eq!(fs::read(dest.join("shdeps")).unwrap(), b"binary");
        assert_eq!(fs::read(dest.join("man/man1/shdeps.1")).unwrap(), b"man");
        assert!(
            fs::metadata(dest.join("shdeps"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111
                != 0
        );
        assert!(extracted.contains(&PathBuf::from("shdeps")));
        assert!(extracted.contains(&PathBuf::from("man/man1/shdeps.1")));
    }

    #[test]
    fn unpack_zip_rejects_traversal_absolute_and_backslash_paths() {
        let dest = temp_dir("zip-paths");

        let traversal =
            unpack_zip(&zip(&[ZipEntry::file("../escape", b"x", 0o644)]), &dest).unwrap_err();
        let absolute =
            unpack_zip(&zip(&[ZipEntry::file("/tmp/escape", b"x", 0o644)]), &dest).unwrap_err();
        let backslash =
            unpack_zip(&zip(&[ZipEntry::file("bin\\tool", b"x", 0o755)]), &dest).unwrap_err();

        assert_eq!(traversal.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(absolute.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(backslash.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn unpack_zip_rejects_symlink_entries() {
        let dest = temp_dir("zip-link");
        let error =
            unpack_zip(&zip(&[ZipEntry::symlink("shdeps-link", "shdeps")]), &dest).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    enum Entry<'a> {
        File {
            path: &'a str,
            body: &'a [u8],
        },
        Link {
            path: &'a str,
            target: &'a str,
            symbolic: bool,
        },
        Directory {
            path: &'a str,
        },
    }

    impl<'a> Entry<'a> {
        fn file(path: &'a str, body: &'a [u8]) -> Self {
            Self::File { path, body }
        }

        fn link(path: &'a str, target: &'a str, symbolic: bool) -> Self {
            Self::Link {
                path,
                target,
                symbolic,
            }
        }

        fn directory(path: &'a str) -> Self {
            Self::Directory { path }
        }
    }

    fn tar_gz(entries: &[Entry<'_>]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_bz2(entries: &[Entry<'_>]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = BzEncoder::new(Vec::new(), BzCompression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_zst(entries: &[Entry<'_>]) -> Vec<u8> {
        let tar = tar(entries);
        zstd::stream::encode_all(tar.as_slice(), 0).unwrap()
    }

    fn tar_xz(entries: &[Entry<'_>]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for entry in entries {
                match entry {
                    Entry::File { path, body } => {
                        let mut header = Header::new_gnu();
                        header.set_path(path).unwrap();
                        header.set_size(body.len() as u64);
                        header.set_mode(0o755);
                        header.set_cksum();
                        builder.append(&header, *body).unwrap();
                    }
                    Entry::Link {
                        path,
                        target,
                        symbolic,
                    } => {
                        let mut header = Header::new_gnu();
                        header.set_path(path).unwrap();
                        header.set_entry_type(if *symbolic {
                            EntryType::Symlink
                        } else {
                            EntryType::Link
                        });
                        header.set_link_name(target).unwrap();
                        header.set_size(0);
                        header.set_cksum();
                        builder.append(&header, std::io::empty()).unwrap();
                    }
                    Entry::Directory { path } => {
                        let mut header = Header::new_gnu();
                        header.set_path(path).unwrap();
                        header.set_entry_type(EntryType::Directory);
                        header.set_size(0);
                        header.set_mode(0o755);
                        header.set_cksum();
                        builder.append(&header, std::io::empty()).unwrap();
                    }
                }
            }
            builder.finish().unwrap();
        }
        tar
    }

    struct ZipEntry<'a> {
        path: &'a str,
        body: &'a [u8],
        mode: u32,
    }

    impl<'a> ZipEntry<'a> {
        fn file(path: &'a str, body: &'a [u8], mode: u32) -> Self {
            Self { path, body, mode }
        }

        fn symlink(path: &'a str, target: &'a str) -> Self {
            Self {
                path,
                body: target.as_bytes(),
                mode: 0o120777,
            }
        }
    }

    fn zip(entries: &[ZipEntry<'_>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = Cursor::new(&mut bytes);
            let mut writer = ZipWriter::new(cursor);
            for entry in entries {
                let options = SimpleFileOptions::default().unix_permissions(entry.mode);
                writer.start_file(entry.path, options).unwrap();
                writer.write_all(entry.body).unwrap();
            }
            writer.finish().unwrap();
        }
        patch_special_zip_modes(&mut bytes, entries);
        bytes
    }

    fn patch_special_zip_modes(bytes: &mut [u8], entries: &[ZipEntry<'_>]) {
        let mut offset = 0;
        let mut index = 0;
        while let Some(relative) = bytes[offset..]
            .windows(4)
            .position(|window| window == b"\x50\x4b\x01\x02")
        {
            let header = offset + relative;
            if let Some(entry) = entries.get(index) {
                if entry.mode & 0o170000 != 0 {
                    // `zip::write::FileOptions::unix_permissions` deliberately
                    // strips the file-type bits, which is normally correct for
                    // regular files. This fixture needs a central-directory
                    // symlink bit so the reader side exercises the production
                    // rejection path for hostile release archives.
                    let external_attributes = (entry.mode << 16).to_le_bytes();
                    bytes[header + 38..header + 42].copy_from_slice(&external_attributes);
                }
            }
            let name_len = u16::from_le_bytes([bytes[header + 28], bytes[header + 29]]) as usize;
            let extra_len = u16::from_le_bytes([bytes[header + 30], bytes[header + 31]]) as usize;
            let comment_len = u16::from_le_bytes([bytes[header + 32], bytes[header + 33]]) as usize;
            offset = header + 46 + name_len + extra_len + comment_len;
            index += 1;
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-archive-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
