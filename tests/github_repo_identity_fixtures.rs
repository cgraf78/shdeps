use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

const ACTIONS_FIXTURE_SHA256: &str =
    "e35531d070dc448aa966976a2827073d2839ad1ee72086889091092a5db867a1";

fn fixture_bytes() -> Vec<u8> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github-repo-identity-v1.tsv");
    fs::read(&path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn actions_owned_github_identity_fixture_bytes_do_not_drift() {
    let bytes = fixture_bytes();
    assert_eq!(
        sha256_hex(&bytes),
        ACTIONS_FIXTURE_SHA256,
        "the GitHub identity fixture must change only with its Actions owner"
    );
}

#[test]
fn canonical_github_repo_matches_shared_actions_vectors() {
    let bytes = fixture_bytes();
    let fixture = std::str::from_utf8(&bytes).expect("fixture must be UTF-8");

    for (index, line) in fixture.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 3, "fixture line {}", index + 1);
        let actual = shdeps::repo::canonical_github_repo(fields[1]);
        match fields[0] {
            "accept" => assert_eq!(actual.as_deref(), Some(fields[2]), "{}", fields[1]),
            "reject" => assert_eq!(actual, None, "{}", fields[1]),
            other => panic!("unknown disposition `{other}` on line {}", index + 1),
        }
    }
}
