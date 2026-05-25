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
}

/// Production HTTP client backed by the host `curl` command.
#[derive(Debug, Clone, Copy, Default)]
pub struct Curl;

impl Client for Curl {
    fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
        let config = curl_config(url, token)?;
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

fn curl_config(url: &str, token: Option<&str>) -> io::Result<String> {
    let mut config = String::new();
    push_config_line(&mut config, "url", url)?;
    push_config_line(&mut config, "header", "User-Agent: shdeps")?;
    push_config_line(&mut config, "header", "Accept: application/vnd.github+json")?;
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        push_config_line(
            &mut config,
            "header",
            &format!("Authorization: Bearer {}", token.trim()),
        )?;
    }
    Ok(config)
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
    use super::curl_config;

    #[test]
    fn curl_config_keeps_token_in_stdin_config_not_process_args() {
        let config = curl_config(
            "https://api.github.com/repos/cgraf78/shdeps/releases",
            Some(" t "),
        )
        .unwrap();

        assert!(config.contains("url = \"https://api.github.com/repos/cgraf78/shdeps/releases\""));
        assert!(config.contains("header = \"Authorization: Bearer t\""));
        assert!(config.contains("header = \"User-Agent: shdeps\""));
    }

    #[test]
    fn curl_config_escapes_quotes_and_backslashes() {
        let config = curl_config("https://example/path\"with\\chars", None).unwrap();

        assert!(config.contains("url = \"https://example/path\\\"with\\\\chars\""));
    }

    #[test]
    fn curl_config_rejects_multiline_values() {
        let error = curl_config("https://example/\nheader = bad", None).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
