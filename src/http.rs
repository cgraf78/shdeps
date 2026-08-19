//! Small HTTP download boundary.
//!
//! Release `self-update` needs network bytes, but the core update planner
//! should stay testable without the network. This module hides the production
//! transport behind a trait and uses `curl` in a way that avoids putting GitHub
//! tokens in process arguments. The token still reaches curl as a header, but
//! via stdin config rather than the command line that other local processes can
//! inspect with `ps`.

use std::io::{self, Write};
use std::process::{Command, Output, Stdio};

/// HTTP client interface used by installer and self-update workflows.
pub trait Client: Sync {
    /// Downloads `url`, optionally adding a GitHub bearer token.
    fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>>;

    /// Downloads a GitHub release asset through the REST asset endpoint.
    fn get_github_asset(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        self.get(url, token)
    }

    /// Returns the target of one unauthenticated HTTP redirect without
    /// following it.
    ///
    /// Test clients default to "unsupported" so existing fakes keep their
    /// narrow request model. Callers must treat `None` as a cache miss and use
    /// their authoritative fallback.
    fn redirect_location(&self, _url: &str) -> io::Result<Option<String>> {
        Ok(None)
    }
}

/// Error payload carrying the final HTTP status of a failed request.
///
/// The transport keeps returning `io::Error` so the `Client` trait and its
/// many test fakes stay untouched; callers that need to distinguish "rate
/// limited" from "missing repo" downcast via `io::Error::get_ref`. Prose
/// stays in `detail`; control flow must use `status()` only.
#[derive(Debug)]
pub struct HttpStatusError {
    status: u16,
    detail: String,
}

impl HttpStatusError {
    /// Creates a payload for a response with `status` and diagnostic prose.
    pub(crate) fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    /// Final HTTP status code observed by the transport.
    pub fn status(&self) -> u16 {
        self.status
    }
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "HTTP {}: {}", self.status, self.detail)
    }
}

impl std::error::Error for HttpStatusError {}

/// Production HTTP client backed by the host `curl` command.
#[derive(Debug, Clone, Copy, Default)]
pub struct Curl;

/// Stall guards applied to every outbound request.
///
/// curl waits forever for a response body that never arrives, so a transfer
/// that connects and then goes quiet blocks the whole run: observed CI stalls
/// spent 35+ minutes inside a single request with no output and no failure.
/// A fixed `--max-time` deadline cannot express the intent here, because large
/// release assets are legitimately slow; `--speed-limit` with `--speed-time`
/// aborts only a transfer that has actually stopped making progress.
///
/// `--retry` covers the transient statuses curl recognizes on its own. It does
/// not mask a rate limit from the GitHub circuit breaker: 403 and 429 responses
/// arrive as completed HTTP transactions, and `--retry` without
/// `--retry-all-errors` leaves those for `github_gate` to observe.
///
/// `run_curl` prepends these to every invocation so the policy has one owner
/// and cannot drift between the GET and redirect paths.
const CURL_STALL_ARGS: [&str; 8] = [
    "--connect-timeout",
    "10",
    "--speed-limit",
    "1024",
    "--speed-time",
    "60",
    "--retry",
    "3",
];

const CURL_GET_ARGS: [&str; 6] = [
    "--fail",
    "--silent",
    "--show-error",
    "--location",
    "--config",
    "-",
];
const CURL_REDIRECT_ARGS: [&str; 4] = ["--silent", "--show-error", "--config", "-"];

impl Client for Curl {
    fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        self.get_with_accept(url, token, GithubAccept::Json)
    }

    fn get_github_asset(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        // Defense in depth: the upstream `github::download_asset` already
        // validates the API URL host before reaching here, but this is the
        // only place where the bearer token actually goes on the wire as
        // an Authorization header. Refusing non-api.github.com URLs here
        // means a future caller that skips the github.rs guard cannot
        // accidentally leak the token to a third-party host. Returning an
        // `InvalidInput` error before spawning curl also avoids any
        // exposure of the token to the child process environment.
        if !is_github_api_url(url) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing authenticated GitHub asset download from non-API host: {url}"),
            ));
        }
        self.get_with_accept(url, token, GithubAccept::OctetStream)
    }

    fn redirect_location(&self, url: &str) -> io::Result<Option<String>> {
        let config = redirect_curl_config(url)?;
        let output = run_curl(&CURL_REDIRECT_ARGS, &config)?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(detail));
        }

        let location = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((!location.is_empty()).then_some(location))
    }
}

impl Curl {
    fn get_with_accept(
        &self,
        url: &str,
        token: Option<&str>,
        accept: GithubAccept,
    ) -> io::Result<Vec<u8>> {
        let config = curl_config(url, token, accept)?;
        let output = run_curl(&CURL_GET_ARGS, &config)?;
        let has_status_trailer = output.status.success() || output.status.code() == Some(22);
        let (body, status) = if has_status_trailer {
            split_status_suffix(output.stdout)
        } else {
            (output.stdout, None)
        };
        if output.status.success() {
            return Ok(body);
        }

        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // Attach the HTTP status only when curl actually saw a response
        // (status "000" means DNS/connect failure - keep the legacy shape so
        // callers do not misclassify network errors as HTTP errors).
        if let Some(status) = status.filter(|&status| status >= 400) {
            return Err(io::Error::other(HttpStatusError::new(status, detail)));
        }
        Err(io::Error::other(detail))
    }
}

fn run_curl(args: &[&str], config: &str) -> io::Result<Output> {
    let mut child = Command::new("curl")
        .args(CURL_STALL_ARGS)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes())?;
    }

    child.wait_with_output()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubAccept {
    Json,
    OctetStream,
}

impl GithubAccept {
    const fn header(self) -> &'static str {
        match self {
            Self::Json => "Accept: application/vnd.github+json",
            Self::OctetStream => "Accept: application/octet-stream",
        }
    }
}

fn curl_config(url: &str, token: Option<&str>, accept: GithubAccept) -> io::Result<String> {
    let mut config = String::new();
    push_config_line(&mut config, "url", url)?;
    push_config_line(&mut config, "user-agent", "shdeps")?;
    // Trailer used by split_status_suffix to recover the final HTTP status
    // even under --fail (which suppresses error bodies but not --write-out).
    config.push_str("write-out = \"\\n%{http_code}\\n\"\n");
    if is_github_api_url(url) {
        push_config_line(&mut config, "header", accept.header())?;
        if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
            push_config_line(
                &mut config,
                "header",
                &format!("Authorization: Bearer {}", token.trim()),
            )?;
        }
    }
    Ok(config)
}

fn redirect_curl_config(url: &str) -> io::Result<String> {
    let mut config = String::new();
    push_config_line(&mut config, "url", url)?;
    push_config_line(&mut config, "user-agent", "shdeps")?;
    // `%{redirect_url}` gives the next hop without following it. Omitting
    // `--location` is intentional: following the redirect would lose the tag
    // identity that this cheap probe is meant to discover.
    config.push_str("output = \"/dev/null\"\n");
    config.push_str("write-out = \"%{redirect_url}\"\n");
    Ok(config)
}

pub(crate) fn is_github_api_url(url: &str) -> bool {
    url.starts_with("https://api.github.com/")
}

fn push_config_line(config: &mut String, key: &str, value: &str) -> io::Result<()> {
    config.push_str(key);
    config.push_str(" = \"");
    config.push_str(&curl_quote(value)?);
    config.push_str("\"\n");
    Ok(())
}

fn curl_quote(value: &str) -> io::Result<String> {
    let mut quoted = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\r' | '\n' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "curl config values must be single-line",
                ));
            }
            _ => quoted.push(ch),
        }
    }
    Ok(quoted)
}

/// Splits the `--write-out "\n%{http_code}\n"` trailer off curl stdout.
///
/// `%{http_code}` always renders as exactly three digits ("000" when no
/// response was received), so the trailer is a fixed 5-byte shape. When the
/// shape is absent (unexpected transport behavior) the output is returned
/// unchanged so behavior degrades to the pre-trailer contract.
fn split_status_suffix(mut stdout: Vec<u8>) -> (Vec<u8>, Option<u16>) {
    let len = stdout.len();
    if len >= 5
        && stdout[len - 5] == b'\n'
        && stdout[len - 1] == b'\n'
        && stdout[len - 4..len - 1].iter().all(u8::is_ascii_digit)
    {
        let status = std::str::from_utf8(&stdout[len - 4..len - 1])
            .ok()
            .and_then(|digits| digits.parse::<u16>().ok());
        stdout.truncate(len - 5);
        return (stdout, status);
    }
    (stdout, None)
}

#[cfg(test)]
mod tests {
    use super::{
        CURL_GET_ARGS, CURL_REDIRECT_ARGS, CURL_STALL_ARGS, GithubAccept, curl_config,
        redirect_curl_config,
    };

    #[test]
    fn curl_get_github_asset_refuses_non_api_github_host() {
        // Defense in depth: if a future caller forgets to validate
        // `api_url` before reaching `get_github_asset`, the http layer
        // itself must refuse the request before the bearer can ever
        // reach the child curl process.
        use super::{Client, Curl};
        let curl = Curl;
        let error = curl
            .get_github_asset("https://attacker.example/api/assets/1", Some("token"))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn curl_config_keeps_api_token_in_stdin_config_not_process_args() {
        let config = curl_config(
            "https://api.github.com/repos/cgraf78/shdeps/releases",
            Some(" t "),
            GithubAccept::Json,
        )
        .unwrap();

        assert!(config.contains("url = \"https://api.github.com/repos/cgraf78/shdeps/releases\""));
        assert!(config.contains("header = \"Authorization: Bearer t\""));
        assert!(config.contains("header = \"Accept: application/vnd.github+json\""));
        assert!(config.contains("user-agent = \"shdeps\""));
        assert!(!config.contains("header = \"User-Agent: shdeps\""));
    }

    #[test]
    fn curl_config_uses_octet_stream_for_github_asset_api_downloads() {
        let config = curl_config(
            "https://api.github.com/repos/owner/repo/releases/assets/123",
            Some("t"),
            GithubAccept::OctetStream,
        )
        .unwrap();

        assert!(config.contains("header = \"Accept: application/octet-stream\""));
        assert!(config.contains("header = \"Authorization: Bearer t\""));
        assert!(!config.contains("Accept: application/vnd.github+json"));
    }

    #[test]
    fn curl_config_omits_github_api_headers_for_asset_downloads() {
        let config = curl_config(
            "https://github.com/owner/repo/releases/download/v1.0.0/tool.tar.gz",
            Some("t"),
            GithubAccept::Json,
        )
        .unwrap();

        assert!(config.contains("user-agent = \"shdeps\""));
        assert!(config.contains("write-out = \"\\n%{http_code}\\n\""));
        assert!(!config.contains("header = \"User-Agent: shdeps\""));
        assert!(!config.contains("Authorization: Bearer"));
        assert!(!config.contains("Accept: application/vnd.github+json"));
    }

    #[test]
    fn curl_config_escapes_quotes_and_backslashes() {
        let config = curl_config(
            "https://example/path\"with\\chars",
            None,
            GithubAccept::Json,
        )
        .unwrap();

        assert!(config.contains("url = \"https://example/path\\\"with\\\\chars\""));
    }

    #[test]
    fn curl_config_rejects_multiline_values() {
        let error =
            curl_config("https://example/\nheader = bad", None, GithubAccept::Json).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn split_status_suffix_strips_write_out_trailer_and_parses_status() {
        let (body, status) = super::split_status_suffix(b"payload\n200\n".to_vec());
        assert_eq!(body, b"payload");
        assert_eq!(status, Some(200));
    }

    #[test]
    fn split_status_suffix_handles_empty_failed_body() {
        // With --fail, curl suppresses the error body; only the trailer remains.
        let (body, status) = super::split_status_suffix(b"\n403\n".to_vec());
        assert!(body.is_empty());
        assert_eq!(status, Some(403));
    }

    #[test]
    fn split_status_suffix_leaves_unexpected_output_untouched() {
        let (body, status) = super::split_status_suffix(b"no trailer here".to_vec());
        assert_eq!(body, b"no trailer here");
        assert_eq!(status, None);
    }

    #[test]
    fn curl_config_appends_write_out_status_trailer() {
        let config = curl_config(
            "https://api.github.com/repos/owner/tool/releases",
            None,
            GithubAccept::Json,
        )
        .unwrap();
        assert!(config.contains("write-out = \"\\n%{http_code}\\n\""));
    }

    #[test]
    fn redirect_curl_config_does_not_follow_or_authenticate() {
        let config = redirect_curl_config("https://github.com/owner/tool/releases/latest").unwrap();

        assert!(config.contains("output = \"/dev/null\""));
        assert!(config.contains("write-out = \"%{redirect_url}\""));
        assert!(!config.contains("Authorization: Bearer"));
        assert!(!CURL_REDIRECT_ARGS.contains(&"--location"));
    }

    #[test]
    fn curl_stall_args_bound_connect_and_stalled_transfers() {
        // Every request is spawned through `run_curl`, which prepends these, so
        // asserting the set here covers the GET and redirect paths at once.
        assert!(CURL_STALL_ARGS.contains(&"--connect-timeout"));
        assert!(CURL_STALL_ARGS.contains(&"--speed-limit"));
        assert!(CURL_STALL_ARGS.contains(&"--speed-time"));

        // A fixed deadline would kill legitimately slow large release assets;
        // the speed guard must stay the mechanism instead.
        assert!(!CURL_STALL_ARGS.contains(&"--max-time"));

        // `--retry-all-errors` would retry 403/429 responses and hide rate
        // limits from the github_gate circuit breaker.
        assert!(!CURL_STALL_ARGS.contains(&"--retry-all-errors"));

        // The per-call arrays must not restate the policy.
        assert!(!CURL_GET_ARGS.contains(&"--connect-timeout"));
        assert!(!CURL_REDIRECT_ARGS.contains(&"--connect-timeout"));
    }

    #[test]
    fn http_status_error_exposes_status_and_detail() {
        use super::HttpStatusError;
        let error = HttpStatusError::new(403, "quota exhausted");
        assert_eq!(error.status(), 403);
        assert_eq!(error.to_string(), "HTTP 403: quota exhausted");
    }
}
