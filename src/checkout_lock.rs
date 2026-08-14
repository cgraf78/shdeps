//! Shared checkout mutation-lock protocol primitives.
//!
//! The generated checkout installer and Shdeps can both update the same
//! `github:repo` root. Their lock is therefore a wire protocol rather than an
//! implementation detail. This module starts with the exact parser and wire
//! transformations so later filesystem arbitration is built on bytes already
//! proven compatible with the Actions-owned conformance fixtures.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

const HEADER: &str = "cgraf78 checkout mutation lock v1";
const OWNER_ROLE_HEX: &str = "6f776e6572";
const CLAIM_ROLE_HEX: &str = "636c61696d";
const PROC_STAT_KIND_HEX: &str = "70726f632d73746174";
const PS_LSTART_KIND_HEX: &str = "70732d6c7374617274";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Owner,
    Claim,
}

impl Role {
    // Return the exact lowercase-hex role value committed to the v1 wire format.
    fn wire_hex(self) -> &'static str {
        match self {
            Self::Owner => OWNER_ROLE_HEX,
            Self::Claim => CLAIM_ROLE_HEX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    role: Role,
    nonce: String,
    owner_nonce: Option<String>,
    pid: String,
    host_hex: String,
    start_kind_hex: String,
    start_token_hex: String,
    checkout_hex: String,
}

// Encode raw bytes directly because checkout paths need not be Unicode.
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// Return the exact checkout identity used by both protocol implementations.
fn checkout_hex(checkout: &Path) -> String {
    #[cfg(unix)]
    {
        encode_hex(checkout.as_os_str().as_bytes())
    }

    #[cfg(not(unix))]
    {
        encode_hex(checkout.as_os_str().to_string_lossy().as_bytes())
    }
}

// Validate one bounded, nonempty lowercase even-length hex field.
fn valid_hex(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.len() % 2 == 0
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

// Validate the 128-bit generation identifier used in path and record names.
fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// Validate a positive decimal PID without imposing a host-sized integer bound.
fn valid_pid(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|first| (b'1'..=b'9').contains(first))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

// Parse one fixed-order record after checking its exact raw-byte framing.
fn parse_record(
    bytes: &[u8],
    expected_role: Role,
    expected_nonce: &str,
    expected_owner_nonce: Option<&str>,
    checkout: &Path,
) -> Result<Record, &'static str> {
    if bytes.contains(&0) || !bytes.ends_with(b"\n") {
        return Err("record framing is invalid");
    }

    let lines = bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    if lines.len() != 9 {
        return Err("record must contain exactly nine lines");
    }
    if lines[0] != HEADER.as_bytes() {
        return Err("record header is invalid");
    }

    let role = field(lines[1], b"role=")?;
    let nonce = field(lines[2], b"nonce=")?;
    let owner_nonce = field(lines[3], b"owner_nonce=")?;
    let pid = field(lines[4], b"pid=")?;
    let host_hex = field(lines[5], b"host_hex=")?;
    let start_kind_hex = field(lines[6], b"start_kind_hex=")?;
    let start_token_hex = field(lines[7], b"start_token_hex=")?;
    let parsed_checkout_hex = field(lines[8], b"checkout_hex=")?;

    if role != expected_role.wire_hex()
        || nonce != expected_nonce
        || owner_nonce != expected_owner_nonce.unwrap_or_default()
    {
        return Err("record identity does not match its path");
    }
    if !valid_nonce(nonce) || !valid_pid(pid) {
        return Err("record nonce or pid is invalid");
    }
    match expected_role {
        Role::Owner if !owner_nonce.is_empty() => {
            return Err("owner records cannot name an owner nonce");
        }
        Role::Claim if !valid_nonce(owner_nonce) => {
            return Err("claim records require a valid owner nonce");
        }
        _ => {}
    }
    if !valid_hex(host_hex, 512) || !valid_hex(start_token_hex, 2048) {
        return Err("record host or process token is invalid");
    }
    if start_kind_hex != PROC_STAT_KIND_HEX && start_kind_hex != PS_LSTART_KIND_HEX {
        return Err("record process backend is unsupported");
    }
    if !valid_hex(parsed_checkout_hex, 8192) || parsed_checkout_hex != checkout_hex(checkout) {
        return Err("record checkout identity is invalid");
    }

    Ok(Record {
        role: expected_role,
        nonce: nonce.to_owned(),
        owner_nonce: (!owner_nonce.is_empty()).then(|| owner_nonce.to_owned()),
        pid: pid.to_owned(),
        host_hex: host_hex.to_owned(),
        start_kind_hex: start_kind_hex.to_owned(),
        start_token_hex: start_token_hex.to_owned(),
        checkout_hex: parsed_checkout_hex.to_owned(),
    })
}

// Extract one fixed field without accepting reordered or duplicate keys.
fn field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a str, &'static str> {
    let value = line
        .strip_prefix(prefix)
        .ok_or("record field is missing or reordered")?;
    std::str::from_utf8(value).map_err(|_| "record field is not ASCII-compatible")
}

// Build the literal relative symlink target, including its defensive `/.` suffix.
fn canonical_target(checkout_name: &str, nonce: &str) -> Option<String> {
    valid_nonce(nonce).then(|| format!(".{checkout_name}.install.lock.owner.{nonce}/."))
}

// Match locale-C awk field normalization for BSD `ps -o lstart=` output.
fn normalize_ps_lstart(input: &[u8]) -> Vec<u8> {
    input
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join(&b' ')
}

// Parse the shared bounded decimal timeout grammar before host arithmetic.
fn parse_timeout(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let normalized = value.trim_start_matches('0');
    let normalized = if normalized.is_empty() {
        "0"
    } else {
        normalized
    };
    (normalized.len() <= 9)
        .then(|| normalized.parse::<u64>().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Role, canonical_target, normalize_ps_lstart, parse_record, parse_timeout};

    const OWNER_RECORD: &[u8] =
        include_bytes!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-owner-record.txt");
    const CLAIM_RECORD: &[u8] =
        include_bytes!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-claim-record.txt");
    const RECORD_VECTORS: &str =
        include_str!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-records.tsv");
    const WIRE_VECTORS: &str =
        include_str!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-wire.tsv");

    // Convert the fixture's lowercase hex bytes without accepting malformed input.
    fn decode_hex(value: &str) -> Vec<u8> {
        assert_eq!(value.len() % 2, 0);
        (0..value.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&value[offset..offset + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_authoritative_owner_and_claim_records_verbatim() {
        assert!(
            parse_record(
                OWNER_RECORD,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_ok()
        );
        assert!(
            parse_record(
                CLAIM_RECORD,
                Role::Claim,
                "fedcba9876543210fedcba9876543210",
                Some("0123456789abcdef0123456789abcdef"),
                Path::new("/tmp/tool"),
            )
            .is_ok()
        );
    }

    #[test]
    fn enforces_every_authoritative_record_vector() {
        for line in RECORD_VECTORS.lines().filter(|line| !line.starts_with('#')) {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 10, "malformed record vector: {line}");
            let role = match fields[1] {
                "owner" => Role::Owner,
                "claim" => Role::Claim,
                _ => Role::Owner,
            };
            let owner_nonce = (fields[3] != "-").then_some(fields[3]);
            let role_hex = match fields[1] {
                "owner" => "6f776e6572",
                "claim" => "636c61696d",
                _ => "6f74686572",
            };
            let record = format!(
                "cgraf78 checkout mutation lock v1\nrole={role_hex}\nnonce={}\nowner_nonce={}\npid={}\nhost_hex={}\nstart_kind_hex={}\nstart_token_hex={}\ncheckout_hex={}\n",
                fields[2],
                owner_nonce.unwrap_or_default(),
                fields[4],
                fields[5],
                fields[6],
                fields[7],
                fields[8]
            );
            let parsed = parse_record(
                record.as_bytes(),
                role,
                fields[2],
                owner_nonce,
                Path::new("/tmp/tool"),
            );

            assert_eq!(
                parsed.is_ok(),
                fields[9] == "valid",
                "record vector disagreed: {}",
                fields[0]
            );
        }
    }

    #[test]
    fn rejects_raw_framing_that_shell_strings_cannot_represent() {
        let mut nul = OWNER_RECORD.to_vec();
        nul.insert(nul.len() - 1, 0);
        assert!(
            parse_record(
                &nul,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_err()
        );

        let without_final_newline = &OWNER_RECORD[..OWNER_RECORD.len() - 1];
        assert!(
            parse_record(
                without_final_newline,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_err()
        );
    }

    #[test]
    fn enforces_authoritative_wire_transformations() {
        for line in WIRE_VECTORS.lines().filter(|line| !line.starts_with('#')) {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 3, "malformed wire vector: {line}");
            match fields[0] {
                "canonical-target" => {
                    let (name, nonce) = fields[1].split_once('|').unwrap();
                    assert_eq!(canonical_target(name, nonce).as_deref(), Some(fields[2]));
                }
                "ps-lstart-normalize" => {
                    assert_eq!(
                        normalize_ps_lstart(&decode_hex(fields[1])),
                        decode_hex(fields[2])
                    );
                }
                other => panic!("unknown wire vector {other}"),
            }
        }
    }

    #[test]
    fn timeout_grammar_normalizes_decimal_without_octal_or_overflow() {
        assert_eq!(parse_timeout("0"), Some(0));
        assert_eq!(parse_timeout("0008"), Some(8));
        assert_eq!(parse_timeout("999999999"), Some(999_999_999));
        assert_eq!(parse_timeout(""), None);
        assert_eq!(parse_timeout(" 8"), None);
        assert_eq!(parse_timeout("1000000000"), None);
    }
}
