//! Build-version reporting for the Rust implementation.
//!
//! The version contract is intentionally small: report the same generated
//! version string used by release tags, archive names, and installer metadata.

/// Git commit embedded into the binary at build time.
///
/// `shdeps` is installed onto machines that may not keep a VERSION file or a
/// full git checkout. Embedding both the public version and the concrete commit
/// during compilation keeps `shdeps version` useful on those machines and
/// prevents the Rust port from falling back to an ambiguous `unknown` string.
pub const COMMIT: &str = env!("SHDEPS_BUILD_COMMIT");

/// Public version embedded into the binary at build time.
///
/// The format is `YYYYMMDD-HHMMSS-<8hex>`. The timestamp makes release assets
/// human-sortable and easy to inspect, while the hash suffix preserves the
/// commit-based identity that mattered for the old source-only Bash install.
pub const VERSION: &str = env!("SHDEPS_BUILD_VERSION");

/// Returns the embedded git commit hash.
#[must_use]
pub const fn commit() -> &'static str {
    COMMIT
}

/// Returns the public generated version string.
#[must_use]
pub const fn version() -> &'static str {
    VERSION
}

/// Returns the compatibility version payload without the leading command name.
#[must_use]
pub const fn description() -> &'static str {
    version()
}

/// Returns the full user-facing `shdeps version` line.
#[must_use]
pub fn line() -> String {
    format!("shdeps {}", description())
}

#[cfg(test)]
mod tests {
    use super::{commit, line, version};

    #[test]
    fn embedded_commit_is_concrete() {
        let commit = commit();

        assert!(commit.len() >= 8);
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(commit, "unknown");
    }

    #[test]
    fn public_version_is_readable_and_traceable() {
        let version = version();
        let parts = version.split('-').collect::<Vec<_>>();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 8);
        assert!(parts[0].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[1].bytes().all(|byte| byte.is_ascii_digit()));
        assert!(parts[2].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(parts[2], &commit()[..8]);
        assert_ne!(version, "unknown");
    }

    #[test]
    fn version_line_uses_public_version() {
        assert_eq!(line(), format!("shdeps {}", version()));
    }
}
