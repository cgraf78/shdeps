use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const FIXTURE_HASHES: &[(&str, &str)] = &[
    (
        "checkout-lock-v1-owner-record.txt",
        "079862d29dd149da06c864a5ddce7881efe7521287b8e9c0bb26c6da1f46ceac",
    ),
    (
        "checkout-lock-v1-claim-record.txt",
        "6cd66571393026e646bef03460b87ebbbcdd11e6680ae9da6618fc201d85312b",
    ),
    (
        "checkout-lock-v1-records.tsv",
        "fe5a79468ec4805d3a2b46fc4ea01ebbce360b17e990b4344b5c845b28318ad7",
    ),
    (
        "checkout-lock-v1-states.tsv",
        "ed9d71c22ee67933257d94ec14dcf9174ba405a0dba84719bf406372020b82a0",
    ),
    (
        "checkout-lock-v1-wire.tsv",
        "1a1fe2ebc8dfef7792bcde39f9ee2895f1cc0d1dac94c0cc9dd0f3c2890bdd67",
    ),
    (
        "shdeps-root-v1.tsv",
        "dd7cf58ad1f328efcc3e86497857414fff2e86ee596c2eff222feece6e626b76",
    ),
];

// Return the vendored fixture directory without depending on the caller's cwd.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/checkout-lock-v1")
}

// Encode a digest locally so the drift gate does not depend on an external tool.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn actions_owned_checkout_lock_fixture_bytes_do_not_drift() {
    for (name, expected) in FIXTURE_HASHES {
        let path = fixture_dir().join(name);
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!("failed to read shared fixture {}: {error}", path.display())
        });

        assert_eq!(
            sha256_hex(&bytes),
            *expected,
            "{name} diverged from the reviewed cgraf78/actions source bytes"
        );
    }
}

#[test]
fn actions_owned_protocol_document_is_vendored_verbatim() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/checkout-lock-v1.md");
    let bytes = fs::read(&path).unwrap();

    assert_eq!(
        sha256_hex(&bytes),
        "8225713ea70740b0492d584a39169ee9d225143bf4abc37d30e8fa70ceccd13e",
        "the public lock specification must change only with its Actions owner"
    );
}
