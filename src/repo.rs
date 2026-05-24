//! GitHub repository install planning helpers.
//!
//! `github:repo` has several compatibility-sensitive naming conventions:
//! config names are canonical `owner/repo`, local dev clones use the short
//! repo name, and private repositories can fall back from HTTPS clone/pull to
//! the normal GitHub SSH form. Keeping those transformations here avoids
//! re-encoding them in clone, pull, status, and tests.

use std::collections::BTreeMap;

use crate::config;

/// GitHub repository source decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Canonical dependency name, normally `owner/repo`.
    pub name: String,
    /// Short repository name used for dev clone lookup.
    pub short: String,
    /// Environment variable that can override the clone URL.
    pub override_var: String,
    /// Selected clone URL.
    pub url: String,
}

/// Builds the source decision for a `github:repo` dependency.
#[must_use]
pub fn source(name: &str, env: &BTreeMap<String, String>) -> Source {
    let name = canonical(name);
    let short = config::short_name(&name).to_owned();
    let override_var = override_var(&short);
    let url = env
        .get(&override_var)
        .cloned()
        .unwrap_or_else(|| format!("https://github.com/{name}"));

    Source {
        name,
        short,
        override_var,
        url,
    }
}

/// Returns the environment variable name for a short repository name.
#[must_use]
pub fn override_var(short: &str) -> String {
    format!("SHDEPS_{}_REPO", short.replace('-', "_").to_uppercase())
}

/// Converts a GitHub HTTPS URL to the normal SSH remote form.
#[must_use]
pub fn ssh_fallback(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if !safe_github_path(path) {
        return None;
    }
    Some(format!("git@github.com:{path}.git"))
}

fn canonical(name: &str) -> String {
    name.strip_suffix(".git").unwrap_or(name).to_owned()
}

fn safe_github_path(path: &str) -> bool {
    path.contains('/')
        && !path.contains(':')
        && !path.contains(' ')
        && !path.starts_with('/')
        && !path.contains("..")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{override_var, source, ssh_fallback, Source};

    #[test]
    fn source_uses_canonical_name_short_name_and_default_https_url() {
        assert_eq!(
            source("cgraf78/ds.git", &BTreeMap::new()),
            Source {
                name: "cgraf78/ds".to_owned(),
                short: "ds".to_owned(),
                override_var: "SHDEPS_DS_REPO".to_owned(),
                url: "https://github.com/cgraf78/ds".to_owned(),
            }
        );
    }

    #[test]
    fn source_uses_short_name_environment_override() {
        let mut env = BTreeMap::new();
        env.insert(
            "SHDEPS_MY_TOOL_REPO".to_owned(),
            "git@github.com:cgraf78/private-tool.git".to_owned(),
        );

        assert_eq!(
            source("cgraf78/my-tool", &env).url,
            "git@github.com:cgraf78/private-tool.git"
        );
    }

    #[test]
    fn override_var_uppercases_and_replaces_dashes() {
        assert_eq!(override_var("ds"), "SHDEPS_DS_REPO");
        assert_eq!(override_var("my-tool"), "SHDEPS_MY_TOOL_REPO");
    }

    #[test]
    fn ssh_fallback_normalizes_safe_github_https_urls() {
        assert_eq!(
            ssh_fallback("https://github.com/cgraf78/ds"),
            Some("git@github.com:cgraf78/ds.git".to_owned())
        );
        assert_eq!(
            ssh_fallback("https://github.com/cgraf78/ds.git"),
            Some("git@github.com:cgraf78/ds.git".to_owned())
        );
        assert_eq!(
            ssh_fallback("https://github.com/cgraf78/ds.git/"),
            Some("git@github.com:cgraf78/ds.git".to_owned())
        );
    }

    #[test]
    fn ssh_fallback_rejects_non_github_or_unsafe_urls() {
        assert_eq!(ssh_fallback("git@github.com:cgraf78/ds.git"), None);
        assert_eq!(ssh_fallback("https://example.com/cgraf78/ds"), None);
        assert_eq!(ssh_fallback("https://github.com/cgraf78"), None);
        assert_eq!(ssh_fallback("https://github.com/cgraf78/ds:bad"), None);
        assert_eq!(ssh_fallback("https://github.com/cgraf78/../bad"), None);
        assert_eq!(ssh_fallback("https://github.com/cgraf78/my repo"), None);
    }
}
