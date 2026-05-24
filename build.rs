use std::env;
use std::fs;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SHDEPS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
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

    println!("cargo:rustc-env=SHDEPS_BUILD_COMMIT={commit}");
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
        .args(["rev-parse", "--short", "HEAD"])
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
    value.len() >= 7 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn head_ref() -> Option<String> {
    let head = fs::read_to_string(".git/HEAD").ok()?;
    let head = head.trim();
    head.strip_prefix("ref: ").map(str::to_owned)
}
