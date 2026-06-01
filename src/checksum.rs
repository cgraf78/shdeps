//! Checksum parsing and verification.
//!
//! Release installers consume checksum files produced by different tools
//! (`sha256sum`, `sha512sum`, `shasum`, and CI helpers). The formats are
//! simple but slightly inconsistent around whitespace and binary-mode `*`
//! prefixes. This module keeps those compatibility rules away from download
//! and activation code so checksum failure can remain a precise bad-artifact
//! reason.

use sha2::{Digest, Sha256, Sha512};

/// Returns the SHA-256 hex digest for `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

/// Returns the SHA-512 hex digest for `bytes`.
#[must_use]
pub fn sha512_hex(bytes: &[u8]) -> String {
    hex_digest(Sha512::digest(bytes).as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        push_hex(&mut hex, *byte);
    }
    hex
}

/// Parses the expected SHA-256 for `file_name` from checksum file text.
///
/// Only filename-bound lines (the standard `<hash>  <file>` /
/// `<hash> *<file>` formats produced by `sha256sum` and `shasum -a 256`)
/// are accepted. Bare-hash lines are rejected here: accepting a
/// detached digest would let any checksum-file payload match any
/// downloaded asset, silently dropping the per-file binding that the
/// whole verification is supposed to enforce. A mirror that publishes a
/// checksum file without the filename — accidentally or otherwise —
/// must fail verification rather than vacuously pass it.
#[must_use]
pub fn expected_sha256(content: &str, file_name: &str) -> Option<String> {
    expected_hex(content, file_name, 64)
}

fn expected_sha512(content: &str, file_name: &str) -> Option<String> {
    expected_hex(content, file_name, 128)
}

fn expected_hex(content: &str, file_name: &str, hex_len: usize) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(hash) = parse_named_line(line, file_name, hex_len) {
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

/// Returns whether `bytes` match a named SHA-256 or SHA-512 checksum line.
#[must_use]
pub fn verify_any(content: &str, file_name: &str, bytes: &[u8]) -> bool {
    expected_sha256(content, file_name).is_some_and(|expected| sha256_hex(bytes) == expected)
        || expected_sha512(content, file_name).is_some_and(|expected| sha512_hex(bytes) == expected)
}

fn parse_named_line(line: &str, file_name: &str, hex_len: usize) -> Option<String> {
    let (hash, rest) = line.split_once(char::is_whitespace)?;
    let hash = normalize_hash(hash, hex_len)?;
    let candidate = named_checksum_file(rest);

    // Match the checksum entry to the exact archive name so a multi-asset
    // release cannot accidentally verify Linux bytes with a macOS checksum.
    (candidate == file_name).then_some(hash)
}

fn named_checksum_file(value: &str) -> &str {
    let value = value.trim_start();
    let value = value.strip_prefix('*').unwrap_or(value).trim_end();
    value.strip_prefix("./").unwrap_or(value)
}

fn normalize_hash(value: &str, hex_len: usize) -> Option<String> {
    let value = value.trim();
    if value.len() == hex_len && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
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
    use super::{expected_sha256, sha256_hex, sha512_hex, verify, verify_any};

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_SHA512: &str = concat!(
        "cf83e1357eefb8bdf1542850d66d8007",
        "d620e4050b5715dc83f4a921d36ce9ce",
        "47d0d13c5d85f2b0ff8318d2877eec2f",
        "63b931bd47417a81a538327af927da3e"
    );

    #[test]
    fn sha256_hex_returns_lowercase_digest() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(
            sha256_hex(b"shdeps"),
            "36adaef0b06ce88bb2aab1ab09ae0620933bd087cb965071353999e8b93f26d6"
        );
    }

    #[test]
    fn sha512_hex_returns_lowercase_digest() {
        assert_eq!(sha512_hex(b""), EMPTY_SHA512);
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
        assert_eq!(
            expected_sha256(
                &format!("{EMPTY_SHA256}  ./shdeps-v1-linux-x86_64-musl.tar.gz\n"),
                "shdeps-v1-linux-x86_64-musl.tar.gz",
            )
            .as_deref(),
            Some(EMPTY_SHA256)
        );
        assert_eq!(
            expected_sha256(
                &format!("{EMPTY_SHA256} *./shdeps-v1-linux-x86_64-musl.tar.gz\n"),
                "shdeps-v1-linux-x86_64-musl.tar.gz",
            )
            .as_deref(),
            Some(EMPTY_SHA256)
        );
    }

    #[test]
    fn expected_sha256_rejects_bare_hash_to_preserve_filename_binding() {
        // A checksum file that contains only a bare digest (no filename)
        // must NOT verify any asset. Otherwise a mirror that strips
        // filenames (accidentally or maliciously) would let any payload
        // pass verification against any expected name. The strict form
        // protects the per-file binding that the whole verification step
        // is supposed to enforce.
        assert_eq!(
            expected_sha256(EMPTY_SHA256, "archive.tar.gz").as_deref(),
            None
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
    fn verify_rejects_bare_hash_payload_for_any_file() {
        // End-to-end: bytes whose digest matches the bare-hash payload
        // must still NOT verify. The named-line path is the only way to
        // express a per-file expectation in production.
        assert!(!verify(EMPTY_SHA256, "archive.tar.gz", b""));
        assert!(!verify(EMPTY_SHA256, "different.tar.gz", b""));
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
    fn verify_matches_bytes_against_named_digest_line() {
        // The production verify path requires a `<hash>  <file>` line
        // (or the binary-mode `<hash> *<file>` variant). Bare-hash
        // checksum files no longer pass — that case is covered by
        // `verify_rejects_bare_hash_payload_for_any_file`.
        let named = format!("{EMPTY_SHA256}  archive.tar.gz\n");
        assert!(verify(&named, "archive.tar.gz", b""));
        assert!(!verify(&named, "archive.tar.gz", b"not empty"));
        // Wrong filename also fails verification even when the digest
        // happens to match the empty payload.
        assert!(!verify(&named, "different.tar.gz", b""));
    }

    #[test]
    fn verify_any_accepts_sha256_or_sha512_named_lines() {
        let sha256 = format!("{EMPTY_SHA256}  archive.tar.gz\n");
        let sha512 = format!("{EMPTY_SHA512}  archive.tar.gz\n");

        assert!(verify_any(&sha256, "archive.tar.gz", b""));
        assert!(verify_any(&sha512, "archive.tar.gz", b""));
        assert!(!verify_any(&sha512, "wrong.tar.gz", b""));
        assert!(!verify_any(EMPTY_SHA512, "archive.tar.gz", b""));
    }
}
