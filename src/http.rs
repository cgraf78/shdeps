//! Small HTTP download boundary.
//!
//! Release `self-update` needs network bytes, but the core update planner
//! should stay testable without the network. This module hides the production
//! transport behind a trait and uses `curl` in a way that avoids putting GitHub
//! tokens in process arguments. The token still reaches curl as a header, but
//! via stdin config rather than the command line that other local processes can
//! inspect with `ps`.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// HTTP client interface used by installer and self-update workflows.
pub trait Client: Sync {
    /// Downloads `url`, optionally adding a GitHub bearer token.
    fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>>;

    /// Downloads a GitHub release asset through the REST asset endpoint.
    fn get_github_asset(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        self.get(url, token)
    }
}

/// Production HTTP client backed by the host `curl` command.
#[derive(Debug, Clone, Copy, Default)]
pub struct Curl;

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
}

impl Curl {
    fn get_with_accept(
        &self,
        url: &str,
        token: Option<&str>,
        accept: GithubAccept,
    ) -> io::Result<Vec<u8>> {
        let config = curl_config(url, token, accept)?;
        let mut child = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--config",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(config.as_bytes())?;
        }

        let output = child.wait_with_output()?;
        if output.status.success() {
            return Ok(output.stdout);
        }

        Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
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

fn is_github_api_url(url: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{GithubAccept, curl_config};

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
}
