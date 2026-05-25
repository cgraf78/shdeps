use std::env;
use std::fs;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SHDEPS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SHDEPS_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=SHDEPS_BUILD_TIMESTAMP");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_REF");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");
    println!("cargo:rerun-if-changed=scripts/release-version.sh");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    if let Some(head_ref) = head_ref() {
        println!("cargo:rerun-if-changed=.git/{head_ref}");
    }

    let commit = env_commit("SHDEPS_BUILD_COMMIT")
        .or_else(|| env_commit("GITHUB_SHA"))
        .or_else(git_commit)
        .unwrap_or_else(|| {
            panic!(
                "failed to resolve shdeps build commit; set SHDEPS_BUILD_COMMIT \
                 to a concrete git hash when building outside a git checkout"
            );
        });
    let version = build_version(&commit);

    println!("cargo:rustc-env=SHDEPS_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=SHDEPS_BUILD_VERSION={version}");
}

fn env_commit(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    let trimmed = value.trim();
    if valid_commit(trimmed) {
        Some(trimmed.to_owned())
    } else if trimmed.is_empty() {
        None
    } else {
        panic!("{name} must be a concrete git hash, got {trimmed:?}");
    }
}

fn git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    valid_commit(commit).then(|| commit.to_owned())
}

fn valid_commit(value: &str) -> bool {
    value.len() >= 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn head_ref() -> Option<String> {
    let head = fs::read_to_string(".git/HEAD").ok()?;
    let head = head.trim();
    head.strip_prefix("ref: ").map(str::to_owned)
}

fn build_version(commit: &str) -> String {
    // Keep the public version formatter in one shell helper because release
    // tags, archive names, and installer smoke tests need the exact same logic
    // without reimplementing Rust build-script details. Passing the already
    // resolved commit also lets containerized builds avoid extra Git metadata
    // reads when a safe.directory mismatch would otherwise block them.
    let output = Command::new("bash")
        .arg("scripts/release-version.sh")
        .env("SHDEPS_BUILD_COMMIT", commit)
        .output()
        .unwrap_or_else(|error| panic!("failed to run scripts/release-version.sh: {error}"));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("failed to compute shdeps build version: {stderr}");
    }

    let version = String::from_utf8(output.stdout)
        .expect("release-version.sh output should be utf-8")
        .trim()
        .to_owned();
    if !valid_version(&version) {
        panic!("release-version.sh produced invalid version {version:?}");
    }
    version
}

fn valid_version(value: &str) -> bool {
    let mut parts = value.split('-');
    let date = parts.next().unwrap_or_default();
    let time = parts.next().unwrap_or_default();
    let commit = parts.next().unwrap_or_default();

    parts.next().is_none()
        && date.len() == 8
        && time.len() == 6
        && date.bytes().all(|byte| byte.is_ascii_digit())
        && time.bytes().all(|byte| byte.is_ascii_digit())
        && commit.len() == 8
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}
