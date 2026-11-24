use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
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
    println!("cargo:rerun-if-changed=scripts/release-lib.sh");
    println!("cargo:rerun-if-changed=scripts/release.conf");
    watch_git_head();

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
    git_output(&["rev-parse", "HEAD"]).filter(|commit| valid_commit(commit))
}

fn valid_commit(value: &str) -> bool {
    value.len() >= 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Track the checkout's HEAD and the branch storage it points to.
fn watch_git_head() {
    let Some(head) = git_path("HEAD").filter(|path| path.is_file()) else {
        return; // An exported source tree can supply an explicit build commit.
    };
    println!("cargo:rerun-if-changed={}", head.display());
    if git_output(&["rev-parse", "--show-ref-format"]).as_deref() == Some("reftable") {
        watch_reftable();
        return;
    }
    // Git before 2.39 can still report the leaf ref recursively.
    let Some(mut reference) =
        symbolic_target("HEAD").or_else(|| git_output(&["symbolic-ref", "--quiet", "HEAD"]))
    else {
        return;
    };

    // Git resolves the shared branch and packed refs correctly in linked worktrees.
    if let Some(packed) = git_path("packed-refs").filter(|path| path.is_file()) {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
    let mut visited = HashSet::new();
    while visited.insert(reference.clone()) {
        let Some(loose) = git_path(&reference) else {
            return;
        };
        // A packed-only branch gets a loose ref on its next update. Watch the
        // containing directory until then, since Cargo always reruns on missing paths.
        if let Some(existing) = loose.ancestors().find(|path| path.exists()) {
            println!("cargo:rerun-if-changed={}", existing.display());
        }
        let Some(target) = symbolic_target(&reference) else {
            break;
        };
        reference = target;
    }
}

/// Follow one hop so retargeting an intermediate symbolic branch triggers a build.
fn symbolic_target(reference: &str) -> Option<String> {
    git_output(&["symbolic-ref", "--quiet", "--no-recurse", reference])
}

/// Reftable stores branch refs in the common index and linked HEADs in a local index.
fn watch_reftable() {
    const INDEX: &str = "reftable/tables.list";
    let local = git_path(INDEX).filter(|path| path.is_file());
    if let Some(path) = &local {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    if let Some(common) = git_output(&["rev-parse", "--git-common-dir"])
        .map(|directory| PathBuf::from(directory).join(INDEX))
        .filter(|path| path.is_file() && Some(path) != local.as_ref())
    {
        println!("cargo:rerun-if-changed={}", common.display());
    }
}

fn git_path(path: &str) -> Option<PathBuf> {
    git_output(&["rev-parse", "--git-path", path]).map(PathBuf::from)
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = build_command("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
}

/// Prevent shell-backed tools and the Bash formatter from sourcing caller startup files.
fn build_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("BASH_ENV").env_remove("ENV");
    // Cargo runs this script in the crate root. For an exported snapshot,
    // keep Git and the formatter's timestamp lookup from searching parents.
    if !Path::new(".git").exists() {
        command.env("GIT_DIR", ".git");
    }
    command
}

fn build_version(commit: &str) -> String {
    // Keep the public version formatter in one shell helper because release
    // tags, archive names, and installer smoke tests need the exact same logic
    // without reimplementing Rust build-script details. Passing the already
    // resolved commit also lets containerized builds avoid extra Git metadata
    // reads when a safe.directory mismatch would otherwise block them.
    let output = build_command("bash")
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
