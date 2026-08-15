//! GitHub repository install planning helpers.
//!
//! `github:repo` has several compatibility-sensitive naming conventions:
//! config names are canonical `owner/repo`, local dev clones use the short
//! repo name, and private repositories can fall back from HTTPS clone/pull to
//! the normal GitHub SSH form. Keeping those transformations here avoids
//! re-encoding them in clone, pull, status, and tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::config::{self, Entry};
use crate::process;
use crate::process::Runner;

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

/// Normalizes one documented GitHub repository URL to `owner/repository`.
///
/// Adoption uses this deliberately narrow identity grammar before trusting an
/// existing checkout. The host is case-insensitive, while owner and repository
/// spelling remains exact. Ports, URL decoration, alternate SSH users,
/// trailing slashes, and encoded path bytes are rejected so a future transport
/// feature cannot silently broaden today's ownership decision.
#[must_use]
pub fn canonical_github_repo(url: &str) -> Option<String> {
    let path = if let Some(rest) = url.strip_prefix("https://") {
        let (host, path) = rest.split_once('/')?;
        host.eq_ignore_ascii_case("github.com").then_some(path)?
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let (user, host) = authority.split_once('@')?;
        (user == "git" && host.eq_ignore_ascii_case("github.com")).then_some(path)?
    } else if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        host.eq_ignore_ascii_case("github.com").then_some(path)?
    } else {
        return None;
    };

    canonical_github_path(path)
}

fn canonical_github_path(path: &str) -> Option<String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(byte, b'%' | b'?' | b'#' | b':' | b'\\')
        })
    {
        return None;
    }
    let mut components = path.split('/');
    let owner = components.next()?;
    let raw_repository = components.next()?;
    // The optional suffix belongs only to the final path component.
    let repository = raw_repository
        .strip_suffix(".git")
        .unwrap_or(raw_repository);
    if components.next().is_some()
        || !safe_github_component(owner)
        || !safe_github_component(repository)
    {
        return None;
    }
    Some(format!("{owner}/{repository}"))
}

fn safe_github_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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

/// Returns the display version for an installed repository checkout.
///
/// A repository-managed dependency reports a checked-in `VERSION` file when
/// present, matching the Bash implementation. Repositories without one fall
/// back to Git's short commit abbreviation so `list` and verbose `update`
/// output stay compact and consistent.
pub(crate) fn version(root: &Path, runner: &impl Runner) -> Option<String> {
    let version_path = root.join("VERSION");
    if let Ok(version) = fs::read_to_string(&version_path) {
        let version = version.trim_end_matches(['\r', '\n']).to_owned();
        if !version.is_empty() {
            return Some(version);
        }
    }

    runner
        .run(
            "git",
            &[
                "-C",
                &root.display().to_string(),
                "rev-parse",
                "--short",
                "HEAD",
            ],
            None,
        )
        .ok()
        .filter(|output| output.success)
        .and_then(|output| {
            let commit = output.stdout.trim();
            (!commit.is_empty()).then(|| format!("commit {commit}"))
        })
}

/// Returns whether a repo install is missing the explicitly configured command.
pub(crate) fn missing_explicit_command(entry: &Entry, root: &Path) -> bool {
    entry.cmd_explicit && !process::executable_path(&root.join("bin").join(&entry.cmd))
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

    use super::{Source, canonical_github_repo, override_var, source, ssh_fallback};

    #[test]
    fn canonical_github_repo_accepts_only_documented_transport_spellings() {
        for (url, expected) in [
            ("https://github.com/owner/tool", "owner/tool"),
            ("https://GITHUB.COM/Owner/Tool.git", "Owner/Tool"),
            ("ssh://git@github.com/owner/tool", "owner/tool"),
            ("ssh://git@GITHUB.COM/Owner/Tool.git", "Owner/Tool"),
            ("git@github.com:owner/tool", "owner/tool"),
            ("git@GITHUB.COM:Owner/Tool.git", "Owner/Tool"),
        ] {
            assert_eq!(
                canonical_github_repo(url).as_deref(),
                Some(expected),
                "{url}"
            );
        }
    }

    #[test]
    fn canonical_github_repo_rejects_ambiguous_or_extensible_urls() {
        for url in [
            "http://github.com/owner/tool",
            "https://user@github.com/owner/tool",
            "https://github.com:443/owner/tool",
            "https://github.com/owner/tool/",
            "https://github.com/owner/tool?ref=main",
            "https://github.com/owner/tool#main",
            "https://github.com/owner%2ftool",
            "ssh://owner@github.com/owner/tool",
            "ssh://git@github.com:22/owner/tool",
            "git@github.com:owner/tool/extra",
            "git@github.com:owner/../tool",
            "git@example.com:owner/tool",
            "owner/tool",
        ] {
            assert_eq!(canonical_github_repo(url), None, "{url}");
        }
    }

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
