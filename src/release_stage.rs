//! Verified staging for shdeps release archives.
//!
//! This module deliberately stops before activation. Downloading, checksum
//! verification, archive extraction, and required-file validation can all fail
//! without touching the live install, so they belong in a preflight layer that
//! produces either a complete staging directory or a precise bad-artifact
//! reason for the transaction layer to report.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::archive;
use crate::checksum;
use crate::github;
use crate::http::Client;
use crate::release_artifact::Pair;

const REQUIRED_FILES: &[&str] = &[
    "shdeps",
    "shdeps.sh",
    "install.sh",
    "README.md",
    "LICENSE",
    "man/man1/shdeps.1",
    "lua/shdeps.lua",
    "lua/shdeps/core.lua",
    "lua/shdeps/bootstrap.lua",
];

/// Successfully staged release archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    /// Directory containing the unpacked candidate release.
    pub dir: PathBuf,
    /// Release tag represented by this staged archive.
    pub tag: String,
}

/// Failure while preparing a release archive before activation.
#[derive(Debug)]
pub enum Failure {
    /// Archive download failed.
    ArchiveDownload(io::Error),
    /// Checksum download failed.
    ChecksumDownload(io::Error),
    /// Checksum text did not match the archive bytes.
    ChecksumMismatch {
        /// Archive file name whose bytes were rejected.
        archive_name: String,
    },
    /// Archive extraction failed.
    Extract(io::Error),
    /// Candidate archive omitted a required release file.
    MissingRequired {
        /// Required relative path that was absent after extraction.
        path: &'static str,
    },
    /// Staging directory setup failed.
    StageDir(io::Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArchiveDownload(error) => write!(formatter, "archive download failed: {error}"),
            Self::ChecksumDownload(error) => write!(formatter, "checksum download failed: {error}"),
            Self::ChecksumMismatch { archive_name } => {
                write!(formatter, "checksum mismatch for {archive_name}")
            }
            Self::Extract(error) => write!(formatter, "archive extraction failed: {error}"),
            Self::MissingRequired { path } => {
                write!(formatter, "archive missing required file {path}")
            }
            Self::StageDir(error) => write!(formatter, "staging directory setup failed: {error}"),
        }
    }
}

impl std::error::Error for Failure {}

/// Downloads, verifies, and extracts a release archive into a staging directory.
pub fn stage(
    pair: &Pair,
    tag: &str,
    parent: &Path,
    token: Option<&str>,
    client: &impl Client,
) -> Result<Staged, Failure> {
    let archive_bytes = github::download_asset(
        client,
        &pair.archive_url,
        pair.archive_api_url.as_deref(),
        token,
    )
    .map_err(Failure::ArchiveDownload)?;
    let checksum_bytes = github::download_asset(
        client,
        &pair.checksum_url,
        pair.checksum_api_url.as_deref(),
        token,
    )
    .map_err(Failure::ChecksumDownload)?;
    let checksum_text = String::from_utf8_lossy(&checksum_bytes);

    if !checksum::verify(&checksum_text, &pair.archive_name, &archive_bytes) {
        return Err(Failure::ChecksumMismatch {
            archive_name: pair.archive_name.clone(),
        });
    }

    let dir = stage_dir(parent, tag).map_err(Failure::StageDir)?;
    if let Err(error) = archive::unpack_tar_gz(&archive_bytes, &dir) {
        let _ = fs::remove_dir_all(&dir);
        return Err(Failure::Extract(error));
    }
    if let Some(path) = missing_required(&dir) {
        let _ = fs::remove_dir_all(&dir);
        return Err(Failure::MissingRequired { path });
    }

    Ok(Staged {
        dir,
        tag: tag.to_owned(),
    })
}

fn stage_dir(parent: &Path, tag: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = parent.join(format!(
        ".shdeps-stage-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir(&dir)?;
    Ok(dir)
}

fn missing_required(dir: &Path) -> Option<&'static str> {
    REQUIRED_FILES
        .iter()
        .copied()
        .find(|path| !dir.join(path).is_file())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    use super::{Failure, stage};
    use crate::checksum;
    use crate::http::Client;
    use crate::release_artifact::Pair;

    #[test]
    fn stage_downloads_verifies_extracts_and_validates_release() {
        let fixture = Fixture::new("ok");
        let archive = release_archive(&[]);
        let checksum = checksum_text(&fixture.pair.archive_name, &archive);
        let client = FakeClient::new()
            .with(&fixture.pair.archive_url, archive)
            .with(&fixture.pair.checksum_url, checksum.into_bytes());

        let staged = stage(
            &fixture.pair,
            "v2026.05.24",
            &fixture.parent,
            Some("token"),
            &client,
        )
        .unwrap();

        assert_eq!(staged.tag, "v2026.05.24");
        assert_eq!(fs::read(staged.dir.join("shdeps")).unwrap(), b"binary");
        assert_eq!(client.tokens(), vec![None, None]);
    }

    #[test]
    fn stage_rejects_checksum_mismatch_without_creating_candidate() {
        let fixture = Fixture::new("checksum");
        let client = FakeClient::new()
            .with(&fixture.pair.archive_url, release_archive(&[]))
            .with(&fixture.pair.checksum_url, b"bad".to_vec());

        let failure =
            stage(&fixture.pair, "v2026.05.24", &fixture.parent, None, &client).unwrap_err();

        assert!(matches!(failure, Failure::ChecksumMismatch { .. }));
        assert!(fs::read_dir(&fixture.parent).unwrap().next().is_none());
    }

    #[test]
    fn stage_cleans_up_after_bad_archive_or_missing_required_file() {
        let bad_archive = Fixture::new("bad-archive");
        let bad_bytes = b"not a tarball".to_vec();
        let bad_checksum = checksum_text(&bad_archive.pair.archive_name, &bad_bytes);
        let bad_client = FakeClient::new()
            .with(&bad_archive.pair.archive_url, bad_bytes)
            .with(&bad_archive.pair.checksum_url, bad_checksum.into_bytes());

        let bad_failure = stage(
            &bad_archive.pair,
            "v2026.05.24",
            &bad_archive.parent,
            None,
            &bad_client,
        )
        .unwrap_err();

        assert!(matches!(bad_failure, Failure::Extract(_)));
        assert!(fs::read_dir(&bad_archive.parent).unwrap().next().is_none());

        let missing = Fixture::new("missing");
        let archive = release_archive(&["LICENSE"]);
        let checksum = checksum_text(&missing.pair.archive_name, &archive);
        let client = FakeClient::new()
            .with(&missing.pair.archive_url, archive)
            .with(&missing.pair.checksum_url, checksum.into_bytes());

        let missing_failure =
            stage(&missing.pair, "v2026.05.24", &missing.parent, None, &client).unwrap_err();

        assert!(matches!(
            missing_failure,
            Failure::MissingRequired { path: "LICENSE" }
        ));
        assert!(fs::read_dir(&missing.parent).unwrap().next().is_none());
    }

    struct Fixture {
        parent: PathBuf,
        pair: Pair,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let parent = temp_dir(name);
            let archive_name = "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz".to_owned();
            let checksum_name = format!("{archive_name}.sha256");
            Self {
                parent,
                pair: Pair {
                    archive_url: format!(
                        "https://github.com/owner/tool/releases/download/v1/{archive_name}"
                    ),
                    archive_api_url: None,
                    archive_name,
                    checksum_url: format!(
                        "https://github.com/owner/tool/releases/download/v1/{checksum_name}"
                    ),
                    checksum_api_url: None,
                    checksum_name,
                },
            }
        }
    }

    #[derive(Default)]
    struct FakeClient {
        responses: BTreeMap<String, Vec<u8>>,
        tokens: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl FakeClient {
        fn new() -> Self {
            Self::default()
        }

        fn with(mut self, url: &str, body: Vec<u8>) -> Self {
            self.responses.insert(url.to_owned(), body);
            self
        }

        fn tokens(&self) -> Vec<Option<String>> {
            self.tokens.lock().unwrap().clone()
        }
    }

    impl Client for FakeClient {
        fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            self.tokens
                .lock()
                .unwrap()
                .push(token.map(ToOwned::to_owned));
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url.to_owned()))
        }
    }

    fn release_archive(omit: &[&str]) -> Vec<u8> {
        let files = [
            ("shdeps", &b"binary"[..]),
            ("shdeps.sh", &b"shim"[..]),
            ("install.sh", &b"install"[..]),
            ("README.md", &b"readme"[..]),
            ("LICENSE", &b"license"[..]),
            ("man/man1/shdeps.1", &b"man"[..]),
            ("lua/shdeps.lua", &b"return {}"[..]),
            ("lua/shdeps/core.lua", &b"return {}"[..]),
            ("lua/shdeps/bootstrap.lua", &b"return {}"[..]),
        ];
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for (path, body) in files {
                if omit.contains(&path) {
                    continue;
                }
                let mut header = Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append(&header, body).unwrap();
            }
            builder.finish().unwrap();
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn checksum_text(file_name: &str, bytes: &[u8]) -> String {
        format!("{}  {file_name}\n", checksum::sha256_hex(bytes))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-release-stage-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
