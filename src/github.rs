//! Minimal GitHub release API data handling.
//!
//! The installer and release-style `self-update` only need a small, stable
//! subset of GitHub's release JSON: tag identity, draft/prerelease flags, and
//! downloadable asset URLs. Keeping that shape here avoids teaching update
//! planning, asset matching, and download code how GitHub happens to spell its
//! fields, and it gives tests a cheap place to exercise malformed API input
//! without touching the network.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::http::Client;
use crate::process::Runner;
use crate::runtime::Env;
use crate::state;

const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Release asset identity needed by shdeps install/update code.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
pub struct Asset {
    /// GitHub asset name, used for diagnostics and checksum pairing.
    pub name: String,
    /// Browser download URL returned by the GitHub release API.
    pub url: String,
    /// REST API URL for authenticated asset downloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

/// GitHub release data after API parsing and compatibility filtering.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
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

/// Downloads a release asset using browser semantics before API fallback.
///
/// Public GitHub release assets should be fetched from `browser_download_url`
/// without API headers because signed storage redirects reject forwarded raw
/// headers. Private release assets need the REST asset endpoint instead, so use
/// it only after the browser URL fails and a token is available.
///
/// Both URLs originate from the GitHub release JSON, which shdeps fetched but
/// could in principle have been tampered with (malformed payload, a tampered
/// cache file, a MITM on an upstream proxy). Before contacting either URL the
/// caller-supplied hosts are validated against known-good GitHub prefixes so a
/// corrupted asset record cannot redirect the download — or the bearer token
/// — to an arbitrary host. The HTTP layer additionally only attaches the auth
/// header to api.github.com URLs, so this is defense-in-depth against the same
/// confused-deputy class of bug.
pub fn download_asset(
    client: &dyn Client,
    browser_url: &str,
    api_url: Option<&str>,
    token: Option<&str>,
) -> io::Result<Vec<u8>> {
    if !is_safe_release_asset_url(browser_url) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to download release asset from non-GitHub host: {browser_url}"),
        ));
    }
    match client.get(browser_url, None) {
        Ok(bytes) => Ok(bytes),
        Err(browser_error) => {
            let Some((api_url, token)) =
                api_url.zip(token.filter(|token| !token.trim().is_empty()))
            else {
                return Err(browser_error);
            };
            if !is_safe_api_asset_url(api_url) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing authenticated download from non-GitHub API host: {api_url}"),
                ));
            }
            client.get_github_asset(api_url, Some(token))
        }
    }
}

/// True for URLs that look like a legitimate GitHub-issued asset download.
///
/// `browser_download_url` in the GitHub API is canonically
/// `https://github.com/<owner>/<repo>/releases/download/<tag>/<file>`; the
/// 30x redirect into `objects.githubusercontent.com` happens at the HTTP
/// layer where curl follows it transparently, so callers only ever supply
/// the `https://github.com/` form. Any other host implies a corrupted or
/// hostile asset record.
#[must_use]
pub fn is_safe_release_asset_url(url: &str) -> bool {
    // `starts_with` is sufficient here: the prefix includes the scheme and
    // the trailing `/`, so it cannot be subverted by a host like
    // `https://github.com.attacker.example/...`.
    url.starts_with("https://github.com/")
        || url.starts_with("https://objects.githubusercontent.com/")
}

/// True for URLs that look like a legitimate GitHub REST asset endpoint.
///
/// The REST asset URL is always under `https://api.github.com/repos/`; the
/// auth bearer token is only attached when the URL passes this check, so a
/// crafted `api_url` value cannot trick shdeps into sending the bearer to a
/// third-party host.
#[must_use]
pub fn is_safe_api_asset_url(url: &str) -> bool {
    url.starts_with("https://api.github.com/")
}

/// Returns the GitHub API releases URL for `owner/repo`.
#[must_use]
pub fn releases_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases")
}

/// Returns the shared release-metadata cache path for `owner/repo`.
#[must_use]
pub fn releases_cache_path(state_dir: &Path, repo: &str) -> PathBuf {
    state_dir.join(format!("{repo}.github.releases.json"))
}

/// Reads cached release metadata written by the generic `github` resolver.
///
/// The cache is an optimization only. A corrupt or missing file is treated as
/// a miss by callers so a later network fetch can repair it; do not let cached
/// metadata become another source of install failure.
pub fn read_cached_releases(state_dir: &Path, repo: &str) -> Option<Vec<Release>> {
    let content = fs::read_to_string(releases_cache_path(state_dir, repo)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Writes parsed release metadata for reuse by later update phases.
///
/// Bare `github` resolution and `github:release` installation both need the
/// same GitHub release payload. Persisting the parsed model lets one update run
/// share that fact without inventing a wider in-memory planner object.
pub fn write_cached_releases(state_dir: &Path, repo: &str, releases: &[Release]) -> Result<()> {
    let mut content = serde_json::to_string(releases)?;
    content.push('\n');
    state::write_atomic(&releases_cache_path(state_dir, repo), &content)
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
    url: Option<String>,
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
            api_url: self
                .url
                .map(|url| url.trim().to_owned())
                .filter(|url| !url.is_empty()),
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

    use super::{
        Asset, Release, download_asset, fetch_releases, parse_releases, releases_url, token,
    };
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
                    "url": "https://api.github.com/repos/owner/repo/releases/assets/1",
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
                    api_url: Some(
                        "https://api.github.com/repos/owner/repo/releases/assets/1".to_owned(),
                    ),
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
    fn download_asset_uses_browser_url_without_token_first() {
        let client = MapClient::new().with(
            "https://github.com/owner/tool/releases/download/v1/tool",
            b"public".to_vec(),
        );

        let bytes = download_asset(
            &client,
            "https://github.com/owner/tool/releases/download/v1/tool",
            Some("https://api.github.com/repos/owner/tool/releases/assets/1"),
            Some("token"),
        )
        .unwrap();

        assert_eq!(bytes, b"public");
        assert_eq!(
            client.calls(),
            vec![Call {
                kind: "get",
                url: "https://github.com/owner/tool/releases/download/v1/tool".to_owned(),
                token: None,
            }]
        );
    }

    #[test]
    fn download_asset_falls_back_to_api_asset_url_for_private_releases() {
        let client = MapClient::new().with_api(
            "https://api.github.com/repos/owner/tool/releases/assets/1",
            b"private".to_vec(),
        );

        let bytes = download_asset(
            &client,
            "https://github.com/owner/tool/releases/download/v1/private-tool",
            Some("https://api.github.com/repos/owner/tool/releases/assets/1"),
            Some(" token "),
        )
        .unwrap();

        assert_eq!(bytes, b"private");
        assert_eq!(
            client.calls(),
            vec![
                Call {
                    kind: "get",
                    url: "https://github.com/owner/tool/releases/download/v1/private-tool"
                        .to_owned(),
                    token: None,
                },
                Call {
                    kind: "asset",
                    url: "https://api.github.com/repos/owner/tool/releases/assets/1".to_owned(),
                    token: Some(" token ".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn download_asset_refuses_browser_url_outside_github_host() {
        // A tampered or malformed release JSON could plant any URL into the
        // `browser_download_url` field. shdeps must refuse to fetch from
        // unrelated hosts even when the path looks plausible.
        let client = MapClient::new().with("https://evil.example/payload", b"bad".to_vec());

        let error = download_asset(
            &client,
            "https://evil.example/payload",
            Some("https://api.github.com/repos/owner/tool/releases/assets/1"),
            Some("token"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // No GET should have been issued at all — the validation runs before
        // any network call so the bearer token cannot leak even partially.
        assert!(client.calls().is_empty());
    }

    #[test]
    fn download_asset_refuses_api_fallback_outside_github_api_host() {
        // The browser URL is genuine (so the first call is made) but the
        // private-fallback `api_url` points to a third-party host. shdeps
        // must refuse to send the bearer token to that host. The original
        // browser error is surfaced via the validation error instead of the
        // bearer ever being placed on the wire.
        let client = MapClient::new();

        let error = download_asset(
            &client,
            "https://github.com/owner/tool/releases/download/v1/tool.tar.gz",
            Some("https://attacker.example/api"),
            Some("token"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The browser GET attempt was made (and failed with NotFound), but
        // no authenticated asset call to the malicious host was issued.
        let calls = client.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].kind, "get");
        assert_eq!(calls[0].token, None);
    }

    #[test]
    fn download_asset_accepts_objects_githubusercontent_url() {
        // Some upstream tooling references the redirect target host
        // directly (e.g., precomputed signed URLs in third-party mirrors).
        // It must be accepted alongside the canonical `github.com` host.
        let client = MapClient::new().with(
            "https://objects.githubusercontent.com/release/payload",
            b"ok".to_vec(),
        );

        let bytes = download_asset(
            &client,
            "https://objects.githubusercontent.com/release/payload",
            None,
            None,
        )
        .unwrap();

        assert_eq!(bytes, b"ok");
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        kind: &'static str,
        url: String,
        token: Option<String>,
    }

    #[derive(Default)]
    struct MapClient {
        browser: BTreeMap<String, Vec<u8>>,
        api: BTreeMap<String, Vec<u8>>,
        calls: std::sync::Mutex<Vec<Call>>,
    }

    impl MapClient {
        fn new() -> Self {
            Self::default()
        }

        fn with(mut self, url: &str, body: Vec<u8>) -> Self {
            self.browser.insert(url.to_owned(), body);
            self
        }

        fn with_api(mut self, url: &str, body: Vec<u8>) -> Self {
            self.api.insert(url.to_owned(), body);
            self
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Client for MapClient {
        fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.lock().unwrap().push(Call {
                kind: "get",
                url: url.to_owned(),
                token: token.map(ToOwned::to_owned),
            });
            self.browser
                .get(url)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url.to_owned()))
        }

        fn get_github_asset(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            self.calls.lock().unwrap().push(Call {
                kind: "asset",
                url: url.to_owned(),
                token: token.map(ToOwned::to_owned),
            });
            self.api
                .get(url)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url.to_owned()))
        }
    }
}
