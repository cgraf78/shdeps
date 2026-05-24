//! SHA-256 checksum parsing and verification.
//!
//! Release installers consume checksum files produced by different tools
//! (`sha256sum`, `shasum -a 256`, and CI helpers). The formats are simple but
//! slightly inconsistent around whitespace and binary-mode `*` prefixes. This
//! module keeps those compatibility rules away from download and activation
//! code so checksum failure can remain a precise bad-artifact reason.

use sha2::{Digest, Sha256};

/// Returns the SHA-256 hex digest for `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        push_hex(&mut hex, byte);
    }
    hex
}

/// Parses the expected SHA-256 for `file_name` from checksum file text.
#[must_use]
pub fn expected_sha256(content: &str, file_name: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(hash) = parse_named_line(line, file_name) {
            return Some(hash);
        }
        if let Some(hash) = parse_bare_hash(line) {
            return Some(hash);
        }
    }

    None
}

/// Returns whether `bytes` match the expected checksum text for `file_name`.
#[must_use]
pub fn verify(content: &str, file_name: &str, bytes: &[u8]) -> bool {
    expected_sha256(content, file_name).is_some_and(|expected| sha256_hex(bytes) == expected)
}

fn parse_named_line(line: &str, file_name: &str) -> Option<String> {
    let (hash, rest) = line.split_once(char::is_whitespace)?;
    let hash = normalize_hash(hash)?;
    let candidate = rest
        .trim_start()
        .strip_prefix('*')
        .unwrap_or(rest.trim_start());

    // Match the checksum entry to the exact archive name so a multi-asset
    // release cannot accidentally verify Linux bytes with a macOS checksum.
    (candidate == file_name).then_some(hash)
}

fn parse_bare_hash(line: &str) -> Option<String> {
    // A bare hash is useful for simple fixtures and mirrors. Only accept it
    // when the whole non-comment line is exactly one digest; otherwise a
    // malformed named line should fail closed instead of silently matching.
    normalize_hash(line)
}

fn normalize_hash(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value.to_ascii_lowercase())
    } else {
        None
    }
}

fn push_hex(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

#[cfg(test)]
mod tests {
    use super::{expected_sha256, sha256_hex, verify};

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn sha256_hex_returns_lowercase_digest() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(
            sha256_hex(b"shdeps"),
            "36adaef0b06ce88bb2aab1ab09ae0620933bd087cb965071353999e8b93f26d6"
        );
    }

    #[test]
    fn expected_sha256_parses_sha256sum_and_shasum_formats() {
        assert_eq!(
            expected_sha256(
                &format!("{EMPTY_SHA256}  shdeps-v1-linux-x86_64-musl.tar.gz\n"),
                "shdeps-v1-linux-x86_64-musl.tar.gz",
            )
            .as_deref(),
            Some(EMPTY_SHA256)
        );
        assert_eq!(
            expected_sha256(
                &format!("{EMPTY_SHA256} *shdeps-v1-linux-x86_64-musl.tar.gz\n"),
                "shdeps-v1-linux-x86_64-musl.tar.gz",
            )
            .as_deref(),
            Some(EMPTY_SHA256)
        );
    }

    #[test]
    fn expected_sha256_accepts_bare_hash_only_when_unambiguous() {
        assert_eq!(
            expected_sha256(EMPTY_SHA256, "archive.tar.gz").as_deref(),
            Some(EMPTY_SHA256)
        );
        assert_eq!(
            expected_sha256(
                &format!("{EMPTY_SHA256} archive.tar.gz extra"),
                "archive.tar.gz",
            ),
            None
        );
    }

    #[test]
    fn expected_sha256_ignores_comments_and_wrong_filenames() {
        assert_eq!(
            expected_sha256(
                &format!("# generated\n{EMPTY_SHA256}  wrong.tar.gz"),
                "archive.tar.gz",
            ),
            None
        );
    }

    #[test]
    fn verify_matches_bytes_against_expected_digest() {
        assert!(verify(EMPTY_SHA256, "archive.tar.gz", b""));
        assert!(!verify(EMPTY_SHA256, "archive.tar.gz", b"not empty"));
    }
}
