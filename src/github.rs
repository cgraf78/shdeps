//! Minimal GitHub release API data handling.
//!
//! The installer and release-style `self-update` only need a small, stable
//! subset of GitHub's release JSON: tag identity, draft/prerelease flags, and
//! downloadable asset URLs. Keeping that shape here avoids teaching update
//! planning, asset matching, and download code how GitHub happens to spell its
//! fields, and it gives tests a cheap place to exercise malformed API input
//! without touching the network.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::http::Client;
use crate::process::Runner;
use crate::runtime::Env;
use crate::state;
use crate::tool_version;

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
    let token = token(env, runner);
    fetch_releases_with_token(repo, client, token.as_deref())
}

/// Fetches and parses releases using a caller-resolved token.
///
/// Forced updates can touch many GitHub repos. Callers that already know they
/// are entering a remote-heavy phase should resolve `gh auth token` once and
/// pass it through here instead of spawning `gh` once per repository.
pub fn fetch_releases_with_token(
    repo: &str,
    client: &(impl Client + ?Sized),
    token: Option<&str>,
) -> Result<Vec<Release>> {
    let url = releases_url(repo);
    let bytes = client.get(&url, token)?;
    let json = String::from_utf8_lossy(&bytes);
    parse_releases(&json)
}

/// Returns GitHub's public latest stable release tag without using the REST API.
///
/// The human-facing `/releases/latest` route redirects to `/releases/tag/<tag>`
/// for public repositories. That redirect is outside the REST API quota and is
/// enough to prove that an installed release remains current. Callers still use
/// the releases API whenever this probe is unavailable or reports a new tag,
/// because asset selection requires the authoritative JSON payload.
pub fn latest_release_tag(
    repo: &str,
    client: &(impl Client + ?Sized),
) -> io::Result<Option<String>> {
    let Some(location) = client.redirect_location(&latest_release_url(repo))? else {
        return Ok(None);
    };
    Ok(tag_from_latest_redirect(repo, &location))
}

/// Returns the public GitHub latest-release URL for `owner/repo`.
#[must_use]
pub fn latest_release_url(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/latest")
}

/// True when the public latest-release redirect identifies `installed`.
pub fn latest_release_matches(
    repo: &str,
    installed: &str,
    client: &(impl Client + ?Sized),
) -> io::Result<bool> {
    Ok(latest_release_tag(repo, client)?.is_some_and(|tag| installed_matches_tag(installed, &tag)))
}

/// True when a command's reported version identifies the given GitHub tag.
///
/// GitHub projects commonly decorate semantic versions as `v1.2.3`,
/// `rust-v1.2.3`, or `release-1.2.3`, while binaries usually print the bare
/// dotted version. Exact non-semver tags remain supported as well.
#[must_use]
pub fn installed_matches_tag(installed: &str, tag: &str) -> bool {
    comparable_versions(tag)
        .into_iter()
        .any(|candidate| candidate == installed)
}

fn tag_from_latest_redirect(repo: &str, location: &str) -> Option<String> {
    let prefix = format!("https://github.com/{repo}/releases/tag/");
    let encoded = location.strip_prefix(&prefix)?;
    if encoded.is_empty() || encoded.contains('/') || encoded.contains('?') || encoded.contains('#')
    {
        return None;
    }
    percent_decode(encoded)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1)?;
        let low = *bytes.get(index + 2)?;
        decoded.push(hex_value(high)? * 16 + hex_value(low)?);
        index += 3;
    }
    String::from_utf8(decoded)
        .ok()
        .filter(|tag| !tag.is_empty())
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn comparable_versions(tag: &str) -> Vec<String> {
    let tag = tag.trim();
    let mut versions = Vec::new();
    push_unique(&mut versions, tag);
    push_unique(&mut versions, tag.strip_prefix('v').unwrap_or(tag));

    if let Some(dotted) = tool_version::extract(&[tag], "release-tag") {
        push_unique(&mut versions, &dotted);
    }

    versions
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if value.is_empty() || values.iter().any(|existing| existing == value) {
        return;
    }
    values.push(value.to_owned());
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
///
/// The `per_page=100` query parameter raises the page size from
/// GitHub's default of 30 to the API maximum. Without it, projects
/// that publish many releases (active CIs publishing nightly builds,
/// patch-heavy projects, repos with many drafts/prereleases) can have
/// `latest_stable` pick the wrong tag — or no tag at all — because
/// the first 30 releases by publication order might be entirely
/// drafts/prereleases. 100 is the API ceiling per single request;
/// callers that need to scan further must implement RFC 5988 Link
/// pagination, which is intentionally NOT done here because shdeps
/// only needs the most recent stable release.
#[must_use]
pub fn releases_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases?per_page=100")
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

/// Invalidates REST metadata after a public redirect becomes the newer fact.
///
/// A matching latest-release redirect proves the installed tag is current but
/// says nothing about the older cached asset list. Removing that list prevents
/// a later update phase from treating stale assets or a withdrawn release as
/// current-run API data.
pub fn remove_cached_releases(state_dir: &Path, repo: &str) -> io::Result<()> {
    match fs::remove_file(releases_cache_path(state_dir, repo)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Resolves the runtime token used for GitHub API calls.
///
/// `GH_TOKEN` wins because it is the most explicit runtime credential knob for
/// shdeps and mirrors the fix used by related CI jobs to avoid rate limiting.
/// `GITHUB_TOKEN` is a common Actions/default fallback. `gh auth token` is last
/// and interactive-only because it can touch user configuration, wake desktop
/// credential helpers, and may be slower, so warm commands should only pay for
/// it on paths that are already doing network work and are attached to a real
/// terminal. Headless callers that need auth should pass GH_TOKEN/GITHUB_TOKEN.
pub fn token(env: &impl Env, runner: &impl Runner) -> Option<String> {
    env_token(env, "GH_TOKEN")
        .or_else(|| env_token(env, "GITHUB_TOKEN"))
        .or_else(|| gh_token_allowed(env).then(|| gh_token(runner)).flatten())
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

fn gh_token_allowed(env: &impl Env) -> bool {
    matches!(
        env_token(env, "SHDEPS_ALLOW_GH_AUTH_TOKEN").as_deref(),
        Some("1" | "true" | "yes")
    ) || (io::stdin().is_terminal() && io::stdout().is_terminal())
}

/// Classified failure for GitHub API metadata fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailure {
    /// 403/429 from api.github.com: primary or secondary rate limit.
    RateLimited,
    /// 404: repository missing, private without credentials, or renamed.
    NotFound,
    /// Anything else: network/DNS failure, parse error, unexpected status.
    Other,
}

/// Classifies a `fetch_releases*` error by transport status when available.
///
/// GitHub answers exhausted unauthenticated quota on `/releases` with 403
/// (429 for some secondary limits). Genuine permission failures on that
/// endpoint surface to unauthenticated callers as 404 (GitHub hides private
/// repos), so 403/429 is safe to attribute to rate limiting rather than to
/// credentials. Errors without an HTTP status payload stay `Other` so test
/// fakes and network failures keep today's behavior.
pub fn fetch_failure(error: &crate::errors::Error) -> FetchFailure {
    let crate::errors::Error::Io(io_error) = error else {
        return FetchFailure::Other;
    };
    let Some(status) = io_error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<crate::http::HttpStatusError>())
        .map(crate::http::HttpStatusError::status)
    else {
        return FetchFailure::Other;
    };
    match status {
        403 | 429 => FetchFailure::RateLimited,
        404 => FetchFailure::NotFound,
        _ => FetchFailure::Other,
    }
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
        Asset, Release, download_asset, fetch_releases, installed_matches_tag, latest_release_tag,
        latest_release_url, parse_releases, releases_url, token,
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
    fn releases_url_requests_max_page_size_to_cover_active_release_streams() {
        // GitHub's default page size is 30; without `per_page=100`,
        // active projects with many drafts/prereleases at the top of
        // the response can starve `latest_stable` of any usable entry.
        // This test pins the contract so a refactor cannot silently
        // shrink the page size and re-introduce that bug.
        let url = super::releases_url("owner/tool");
        assert!(
            url.starts_with("https://api.github.com/repos/owner/tool/releases"),
            "URL must point at the standard releases endpoint: {url}"
        );
        assert!(
            url.contains("per_page=100"),
            "URL must request the API maximum page size: {url}"
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
    fn latest_release_tag_accepts_only_same_repo_tag_redirects() {
        let client = RedirectClient(Some(
            "https://github.com/owner/tool/releases/tag/release-1.2.3%2Bbuild".to_owned(),
        ));

        assert_eq!(
            latest_release_url("owner/tool"),
            "https://github.com/owner/tool/releases/latest"
        );
        assert_eq!(
            latest_release_tag("owner/tool", &client)
                .unwrap()
                .as_deref(),
            Some("release-1.2.3+build")
        );

        for location in [
            "https://github.com/attacker/tool/releases/tag/v1",
            "https://github.com/owner/tool/releases/tag/",
            "https://github.com/owner/tool/releases/tag/v1/extra",
            "https://github.com/owner/tool/releases/tag/v1?download=1",
            "https://example.test/owner/tool/releases/tag/v1",
        ] {
            assert_eq!(
                latest_release_tag("owner/tool", &RedirectClient(Some(location.to_owned())))
                    .unwrap(),
                None,
                "must reject untrusted redirect {location}"
            );
        }
    }

    #[test]
    fn release_tag_comparison_accepts_common_github_prefixes() {
        assert!(installed_matches_tag("1.2.3", "v1.2.3"));
        assert!(installed_matches_tag("0.133.0", "rust-v0.133.0"));
        assert!(installed_matches_tag("2026.5.15", "release-2026.5.15"));
        assert!(installed_matches_tag(
            "nightly-20260525",
            "nightly-20260525"
        ));
        assert!(!installed_matches_tag("1.2.2", "v1.2.3"));
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
        assert_eq!(
            token(
                &FakeEnv::new().with_var("SHDEPS_ALLOW_GH_AUTH_TOKEN", "1"),
                &runner
            )
            .as_deref(),
            Some("from-gh")
        );
    }

    #[test]
    fn token_skips_gh_cli_in_headless_contexts() {
        assert_eq!(token(&FakeEnv::new(), &PanicRunner).as_deref(), None);
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
        fn exists(&self, command: &str) -> bool {
            panic!("token resolution must not query {command} in this context");
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            timeout: Option<Duration>,
        ) -> io::Result<Output> {
            panic!(
                "token resolution must not run {program} {args:?} with timeout {timeout:?} in this context"
            );
        }
    }

    struct RedirectClient(Option<String>);

    impl Client for RedirectClient {
        fn get(&self, url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            panic!("redirect probe must not issue a body GET for {url}");
        }

        fn redirect_location(&self, url: &str) -> io::Result<Option<String>> {
            assert_eq!(url, "https://github.com/owner/tool/releases/latest");
            Ok(self.0.clone())
        }
    }

    #[test]
    fn fetch_failure_classifies_rate_limit_statuses() {
        use crate::http::HttpStatusError;
        for status in [403u16, 429] {
            let error = crate::errors::Error::Io(std::io::Error::other(HttpStatusError::new(
                status, "limited",
            )));
            assert_eq!(
                super::fetch_failure(&error),
                super::FetchFailure::RateLimited
            );
        }
    }

    #[test]
    fn fetch_failure_classifies_missing_repos() {
        use crate::http::HttpStatusError;
        let error =
            crate::errors::Error::Io(std::io::Error::other(HttpStatusError::new(404, "nope")));
        assert_eq!(super::fetch_failure(&error), super::FetchFailure::NotFound);
    }

    #[test]
    fn fetch_failure_defaults_to_other_without_status_payload() {
        let io_error = crate::errors::Error::Io(std::io::Error::other("curl: connect timeout"));
        assert_eq!(super::fetch_failure(&io_error), super::FetchFailure::Other);

        let json_error = crate::errors::Error::Json(
            serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
        );
        assert_eq!(
            super::fetch_failure(&json_error),
            super::FetchFailure::Other
        );
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
