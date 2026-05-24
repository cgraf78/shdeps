//! Install metadata for release/archive installs.
//!
//! The Rust installer and release-style `self-update` cannot safely infer how
//! shdeps was installed from filesystem shape alone. A source checkout, a
//! converted Bash-era install, and a release archive can all contain a `shdeps`
//! binary and `shdeps.sh`, but their update and rollback rules are different.
//! This module owns the small JSON contract that lets those paths make explicit
//! decisions instead of guessing.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state;
use crate::Result;

/// Install metadata file name stored under `SHDEPS_DIR`.
pub const FILE_NAME: &str = ".shdeps-install.json";

/// Current metadata schema version.
pub const SCHEMA: u64 = 1;

/// Supported installation methods recorded in metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// A git checkout managed with source-checkout self-update behavior.
    Git,
    /// A prebuilt release archive installed by the Rust installer.
    Release,
    /// A local source build installed by bootstrap/install tooling.
    SourceBuild,
    /// A user-managed install; automatic release self-update is not safe.
    Manual,
}

/// Previous install that was converted into the current install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvertedFrom {
    /// Method used by the previous install.
    pub method: Method,
    /// Path to the previous install, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Commit used by the previous install, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Durable install metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// Schema version. Unknown versions must not be interpreted as release
    /// self-update instructions because rollback semantics may have changed.
    pub schema: u64,
    /// Installation method.
    pub method: Method,
    /// Release artifact platform label such as `linux-x86_64-musl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_platform: Option<String>,
    /// Release tag used by the current install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Build commit used by the current install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Source repository, usually `cgraf78/shdeps`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Previous install details for diagnostics and rollback decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<ConvertedFrom>,
    /// Install timestamp as an RFC3339-style string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

impl Metadata {
    /// Creates metadata with the current schema.
    #[must_use]
    pub fn new(method: Method) -> Self {
        Self {
            schema: SCHEMA,
            method,
            artifact_platform: None,
            tag: None,
            commit: None,
            repo: None,
            converted_from: None,
            installed_at: None,
        }
    }
}

/// Result of reading metadata from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// Metadata file is absent.
    Missing,
    /// Metadata parsed and passed schema validation.
    Valid(Metadata),
    /// Metadata exists but is not safe to interpret.
    Invalid {
        /// Human-readable reason suitable for a clear self-update error.
        reason: String,
    },
}

/// Returns the install metadata path for `shdeps_dir`.
#[must_use]
pub fn path(shdeps_dir: &Path) -> PathBuf {
    shdeps_dir.join(FILE_NAME)
}

/// Reads install metadata, treating malformed or future-schema files as invalid.
pub fn read(shdeps_dir: &Path) -> Result<Read> {
    let metadata_path = path(shdeps_dir);
    let content = match fs::read_to_string(&metadata_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Read::Missing),
        Err(error) => return Err(error.into()),
    };

    let metadata = match serde_json::from_str::<Metadata>(&content) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Ok(Read::Invalid {
                reason: format!("invalid install metadata JSON: {error}"),
            });
        }
    };
    if metadata.schema != SCHEMA {
        return Ok(Read::Invalid {
            reason: format!("unsupported install metadata schema {}", metadata.schema),
        });
    }

    Ok(Read::Valid(metadata))
}

/// Writes install metadata atomically.
pub fn write(shdeps_dir: &Path, metadata: &Metadata) -> Result<()> {
    let mut content = serde_json::to_string_pretty(metadata)?;
    content.push('\n');
    state::write_atomic(&path(shdeps_dir), &content)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{path, read, write, ConvertedFrom, Metadata, Method, Read};

    #[test]
    fn missing_metadata_is_explicit() {
        let dir = temp_dir("missing");

        assert_eq!(read(&dir).unwrap(), Read::Missing);
    }

    #[test]
    fn writes_and_reads_release_metadata() {
        let dir = temp_dir("round-trip");
        let mut metadata = Metadata::new(Method::Release);
        metadata.artifact_platform = Some("linux-x86_64-musl".to_owned());
        metadata.tag = Some("v2026.05.23".to_owned());
        metadata.commit = Some("abc1234".to_owned());
        metadata.repo = Some("cgraf78/shdeps".to_owned());
        metadata.converted_from = Some(ConvertedFrom {
            method: Method::Git,
            path: Some(PathBuf::from("/home/chris/.local/share/shdeps")),
            commit: Some("def5678".to_owned()),
        });
        metadata.installed_at = Some("2026-05-23T00:00:00Z".to_owned());

        write(&dir, &metadata).unwrap();

        assert_eq!(read(&dir).unwrap(), Read::Valid(metadata));
        let content = fs::read_to_string(path(&dir)).unwrap();
        assert!(content.ends_with('\n'));
        assert!(content.contains("\"method\": \"release\""));
    }

    #[test]
    fn source_build_method_uses_kebab_case_contract() {
        let dir = temp_dir("source-build");
        write(&dir, &Metadata::new(Method::SourceBuild)).unwrap();

        let content = fs::read_to_string(path(&dir)).unwrap();

        assert!(content.contains("\"method\": \"source-build\""));
        assert_eq!(
            read(&dir).unwrap(),
            Read::Valid(Metadata::new(Method::SourceBuild))
        );
    }

    #[test]
    fn unknown_schema_is_invalid_not_missing() {
        let dir = temp_dir("schema");
        fs::write(path(&dir), r#"{"schema":2,"method":"release"}"#).unwrap();

        assert_eq!(
            read(&dir).unwrap(),
            Read::Invalid {
                reason: "unsupported install metadata schema 2".to_owned()
            }
        );
    }

    #[test]
    fn malformed_or_unknown_method_metadata_is_invalid() {
        let dir = temp_dir("invalid");
        fs::write(path(&dir), r#"{"schema":1,"method":"future"}"#).unwrap();
        let unknown = read(&dir).unwrap();
        assert!(matches!(unknown, Read::Invalid { reason } if reason.contains("unknown variant")));

        fs::write(path(&dir), "{not json").unwrap();
        let malformed = read(&dir).unwrap();
        assert!(
            matches!(malformed, Read::Invalid { reason } if reason.contains("invalid install metadata JSON"))
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-install-metadata-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
