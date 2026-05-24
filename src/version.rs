//! Build-version reporting for the Rust implementation.
//!
//! The version contract is intentionally small: report the concrete git commit
//! embedded by `build.rs`, and fail the build if no commit can be resolved.

/// Git commit embedded into the binary at build time.
///
/// `shdeps` is installed onto machines that may not keep a VERSION file or a
/// full git checkout. Embedding the commit during compilation keeps `shdeps
/// version` useful on those machines and prevents the Rust port from falling
/// back to an ambiguous `unknown` string.
pub const COMMIT: &str = env!("SHDEPS_BUILD_COMMIT");

/// Returns the embedded git commit hash.
#[must_use]
pub const fn commit() -> &'static str {
    COMMIT
}

/// Returns the compatibility version payload without the leading command name.
#[must_use]
pub fn description() -> String {
    format!("commit {}", commit())
}

/// Returns the full user-facing `shdeps version` line.
#[must_use]
pub fn line() -> String {
    format!("shdeps {}", description())
}

#[cfg(test)]
mod tests {
    use super::{commit, line};

    #[test]
    fn embedded_commit_is_concrete() {
        let commit = commit();

        assert!(commit.len() >= 7);
        assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(commit, "unknown");
    }

    #[test]
    fn version_line_matches_bash_shape() {
        assert_eq!(line(), format!("shdeps commit {}", commit()));
    }
}
