//! Safe extraction helpers for shdeps-owned release archives.
//!
//! The Bash reference delegates extraction to `tar`, which preserves broad
//! compatibility but does not give shdeps a chance to reject hostile archive
//! entries before they touch disk. Rust release self-update uses this module
//! for its own artifacts so traversal and link tricks fail closed while normal
//! shdeps release tarballs still extract with their executable bits intact.

use std::fs;
use std::io::{self, Cursor};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

/// Extracts a gzip-compressed tar archive into `dest`.
pub fn unpack_tar_gz(bytes: &[u8], dest: &Path) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dest)?;
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);
    let mut extracted = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let relative = safe_entry_path(entry.path()?.as_ref())?;
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

fn safe_entry_path(path: &Path) -> io::Result<PathBuf> {
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty archive path",
        ));
    }
    Ok(safe)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, EntryType, Header};

    use super::{safe_entry_path, unpack_tar_gz};

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
    fn unpack_tar_gz_rejects_parent_traversal_and_absolute_paths() {
        let traversal = safe_entry_path(std::path::Path::new("../escape")).unwrap_err();
        let absolute = safe_entry_path(std::path::Path::new("/tmp/escape")).unwrap_err();

        assert_eq!(traversal.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(absolute.kind(), std::io::ErrorKind::InvalidData);
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
    }

    fn tar_gz(entries: &[Entry<'_>]) -> Vec<u8> {
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
                }
            }
            builder.finish().unwrap();
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
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
