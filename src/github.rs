//! Minimal GitHub release API data handling.
//!
//! The installer and release-style `self-update` only need a small, stable
//! subset of GitHub's release JSON: tag identity, draft/prerelease flags, and
//! downloadable asset URLs. Keeping that shape here avoids teaching update
//! planning, asset matching, and download code how GitHub happens to spell its
//! fields, and it gives tests a cheap place to exercise malformed API input
//! without touching the network.

use std::time::Duration;

use serde::Deserialize;

use crate::http::Client;
use crate::process::Runner;
use crate::runtime::Env;
use crate::Result;

const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Release asset identity needed by shdeps install/update code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// GitHub asset name, used for diagnostics and checksum pairing.
    pub name: String,
    /// Browser download URL returned by the GitHub release API.
    pub url: String,
}

/// GitHub release data after API parsing and compatibility filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Release tag name from GitHub's `tag_name` field.
    pub tag: String,
    /// True when GitHub marks the release as a draft.
    pub draft: bool,
    /// True when GitHub marks the release as a prerelease.
    pub prerelease: bool,
    /// Downloadable assets attached to the release.
    pub assets: Vec<Asset>,
}

/// Parses GitHub releases JSON into the narrow shdeps model.
///
/// Unknown fields are intentionally ignored because GitHub adds fields over
/// time. Releases without a tag are skipped instead of poisoning the whole
/// response: a tag is the durable version identity that self-update uses for
/// downgrade protection, so an untagged object cannot be acted on safely.
pub fn parse_releases(json: &str) -> Result<Vec<Release>> {
    let releases = serde_json::from_str::<Vec<ApiRelease>>(json)?;
    Ok(releases
        .into_iter()
        .filter_map(ApiRelease::into_release)
        .collect())
}

/// Fetches and parses releases for an `owner/repo` GitHub repository.
pub fn fetch_releases(
    repo: &str,
    env: &impl Env,
    runner: &impl Runner,
    client: &(impl Client + ?Sized),
) -> Result<Vec<Release>> {
    let url = releases_url(repo);
    let token = token(env, runner);
    let bytes = client.get(&url, token.as_deref())?;
    let json = String::from_utf8_lossy(&bytes);
    parse_releases(&json)
}

/// Returns the GitHub API releases URL for `owner/repo`.
#[must_use]
pub fn releases_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases")
}

/// Resolves the runtime token used for GitHub API calls.
///
/// `GH_TOKEN` wins because it is the most explicit runtime credential knob for
/// shdeps and mirrors the fix used by related CI jobs to avoid rate limiting.
/// `GITHUB_TOKEN` is a common Actions/default fallback. `gh auth token` is last
/// because it can touch user configuration and may be slower, so warm commands
/// should only pay for it on paths that are already doing network work.
pub fn token(env: &impl Env, runner: &impl Runner) -> Option<String> {
    env_token(env, "GH_TOKEN")
        .or_else(|| env_token(env, "GITHUB_TOKEN"))
        .or_else(|| gh_token(runner))
}

fn env_token(env: &impl Env, name: &str) -> Option<String> {
    env.var_os(name)
        .and_then(|value| value.into_string().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn gh_token(runner: &impl Runner) -> Option<String> {
    if !runner.exists("gh") {
        return None;
    }

    let output = runner
        .run("gh", &["auth", "token"], Some(GH_TOKEN_TIMEOUT))
        .ok()?;
    if !output.success {
        return None;
    }

    let token = output.stdout.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

impl ApiRelease {
    fn into_release(self) -> Option<Release> {
        let tag = self.tag_name?.trim().to_owned();
        if tag.is_empty() {
            return None;
        }

        Some(Release {
            tag,
            draft: self.draft,
            prerelease: self.prerelease,
            assets: self
                .assets
                .into_iter()
                .filter_map(ApiAsset::into_asset)
                .collect(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    name: Option<String>,
    browser_download_url: Option<String>,
}

impl ApiAsset {
    fn into_asset(self) -> Option<Asset> {
        let url = self.browser_download_url?.trim().to_owned();
        if url.is_empty() {
            return None;
        }

        Some(Asset {
            name: self.name.unwrap_or_default(),
            url,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    use super::{fetch_releases, parse_releases, releases_url, token, Asset, Release};
    use crate::http::Client;
    use crate::process::{Output, Runner};
    use crate::runtime::Env;

    #[test]
    fn parse_releases_keeps_only_safe_release_identity_and_assets() {
        let releases = parse_releases(
            r#"[
              {
                "tag_name": " v2026.05.24 ",
                "draft": false,
                "prerelease": false,
                "extra": "ignored",
                "assets": [
                  {
                    "name": "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz",
                    "browser_download_url": "https://example/archive.tar.gz"
                  },
                  {"name": "missing-url"}
                ]
              },
              {"draft": false, "assets": []},
              {"tag_name": "   ", "assets": []}
            ]"#,
        )
        .unwrap();

        assert_eq!(
            releases,
            vec![Release {
                tag: "v2026.05.24".to_owned(),
                draft: false,
                prerelease: false,
                assets: vec![Asset {
                    name: "shdeps-v2026.05.24-linux-x86_64-musl.tar.gz".to_owned(),
                    url: "https://example/archive.tar.gz".to_owned(),
                }],
            }]
        );
    }

    #[test]
    fn parse_releases_rejects_non_array_api_payloads() {
        let error = parse_releases(r#"{"message":"rate limited"}"#).unwrap_err();

        assert!(error.to_string().contains("invalid type"));
    }

    #[test]
    fn fetch_releases_uses_repo_url_and_resolved_token() {
        let env = FakeEnv::new().with_var("GH_TOKEN", "token");
        let runner = PanicRunner;
        let client = FakeClient {
            expected_url: releases_url("cgraf78/shdeps"),
            expected_token: Some("token".to_owned()),
            body: br#"[{"tag_name":"v2026.05.24","assets":[]}]"#.to_vec(),
        };

        let releases = fetch_releases("cgraf78/shdeps", &env, &runner, &client).unwrap();

        assert_eq!(releases, vec![release("v2026.05.24")]);
    }

    #[test]
    fn token_uses_env_before_gh_cli() {
        let env = FakeEnv::new()
            .with_var("GH_TOKEN", " explicit ")
            .with_var("GITHUB_TOKEN", "fallback");

        assert_eq!(token(&env, &PanicRunner).as_deref(), Some("explicit"));
    }

    #[test]
    fn token_falls_back_to_github_token_then_gh_cli() {
        let runner = FakeRunner::new().with_gh_token("from-gh");

        assert_eq!(
            token(&FakeEnv::new().with_var("GITHUB_TOKEN", "actions"), &runner).as_deref(),
            Some("actions")
        );
        assert_eq!(token(&FakeEnv::new(), &runner).as_deref(), Some("from-gh"));
    }

    #[test]
    fn token_ignores_missing_or_failed_gh_cli() {
        assert_eq!(token(&FakeEnv::new(), &FakeRunner::new()), None);
        assert_eq!(
            token(&FakeEnv::new(), &FakeRunner::new().with_failed_gh()).as_deref(),
            None
        );
    }

    #[derive(Default)]
    struct FakeEnv {
        vars: BTreeMap<String, OsString>,
    }

    impl FakeEnv {
        fn new() -> Self {
            Self::default()
        }

        fn with_var(mut self, name: &str, value: &str) -> Self {
            self.vars.insert(name.to_owned(), OsString::from(value));
            self
        }
    }

    impl Env for FakeEnv {
        fn var_os(&self, name: &str) -> Option<OsString> {
            self.vars.get(name).cloned()
        }

        fn command_output(&self, _command: &str, _args: &[&str]) -> Option<String> {
            None
        }

        fn read_to_string(&self, _path: &Path) -> Option<String> {
            None
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        gh_output: Option<Output>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self::default()
        }

        fn with_gh_token(mut self, token: &str) -> Self {
            self.gh_output = Some(Output {
                success: true,
                timed_out: false,
                stdout: format!("{token}\n"),
                stderr: String::new(),
            });
            self
        }

        fn with_failed_gh(mut self) -> Self {
            self.gh_output = Some(Output {
                success: false,
                timed_out: false,
                stdout: String::new(),
                stderr: "not logged in".to_owned(),
            });
            self
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, command: &str) -> bool {
            command == "gh" && self.gh_output.is_some()
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            timeout: Option<Duration>,
        ) -> io::Result<Output> {
            assert_eq!(program, "gh");
            assert_eq!(args, ["auth", "token"]);
            assert!(timeout.is_some());
            self.gh_output
                .clone()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing gh"))
        }
    }

    struct PanicRunner;

    impl Runner for PanicRunner {
        fn exists(&self, _command: &str) -> bool {
            true
        }

        fn run(
            &self,
            _program: &str,
            _args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            panic!("environment tokens must short-circuit gh credential probing");
        }
    }

    struct FakeClient {
        expected_url: String,
        expected_token: Option<String>,
        body: Vec<u8>,
    }

    impl Client for FakeClient {
        fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            assert_eq!(url, self.expected_url);
            assert_eq!(token, self.expected_token.as_deref());
            Ok(self.body.clone())
        }
    }

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.to_owned(),
            draft: false,
            prerelease: false,
            assets: Vec::new(),
        }
    }
}
