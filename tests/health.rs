//! Contract tests for `shdeps health`.
//!
//! The command is consumed by `dot doctor` across independently updating
//! hosts, so these tests pin the machine contract (exit codes, five TSV
//! columns, kind tokens) and that each problem kind is detected from
//! hand-built state, without network, writes, or locks.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use shdeps::cli::{HELP, PUBLIC_COMMANDS};
use shdeps::health::ProblemKind;

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-health-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace("::", "-")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state")).unwrap();
        let dir = fs::canonicalize(&dir).unwrap();
        // Any network attempt fails loudly instead of reaching GitHub.
        let curl = dir.join("fakebin/curl");
        fs::create_dir_all(curl.parent().unwrap()).unwrap();
        fs::write(&curl, "#!/bin/sh\necho 'unexpected curl' >&2\nexit 99\n").unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn write_executable(&self, rel: &str) -> PathBuf {
        let path = self.write(rel, "#!/bin/sh\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn link(&self, target: &Path, rel: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, &path).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_shdeps"));
        command
            .env_clear()
            .env("HOME", self.path("home"))
            .env("SHDEPS_CONF_DIR", self.path("conf"))
            .env("SHDEPS_STATE_DIR", self.path("state"))
            .env("SHDEPS_GIT_DEV_DIR", self.path("git"))
            .env("SHDEPS_INSTALL_DIR", self.path("share"))
            .env("SHDEPS_BIN_DIR", self.path("bin"))
            .env("SHDEPS_TEST_PLATFORM", "linux")
            .env("SHDEPS_TEST_HOST", "test-host")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("fakebin").display()),
            )
            .args(args);
        command
    }

    fn health(&self) -> Output {
        self.command(&["health"]).output().unwrap()
    }

    /// A healthy archive release: marker, public link into the root, and
    /// tracked bin/extras links.
    fn healthy_release(&self, name: &str, cmd: &str) {
        self.append("conf/deps.conf", &format!("{name} github:release {cmd}\n"));
        let root = format!("share/{name}");
        let binary = self.write_executable(&format!("{root}/bin/{cmd}"));
        self.write(&format!("{root}/.shdeps-release-layout"), "v1 archive\n");
        let man = self.write(&format!("{root}/man/{cmd}.1"), ".TH X 1\n");
        let public = self.link(&binary, &format!("bin/{cmd}"));
        let man_link = self.link(&man, &format!("share/man/man1/{cmd}.1"));
        self.write(
            &format!("state/{name}.binlinks"),
            &format!("{}\n", public.display()),
        );
        self.write(
            &format!("state/{name}.links"),
            &format!("{}\n", man_link.display()),
        );
        self.append(
            "state/manifest",
            &format!("{name}|github:release|{cmd}|{}\n", public.display()),
        );
    }

    /// A healthy repo checkout exposing one command.
    fn healthy_repo(&self, name: &str, cmd: &str) {
        self.append("conf/deps.conf", &format!("{name} github:repo\n"));
        let binary = self.write_executable(&format!("share/{name}/bin/{cmd}"));
        self.link(&binary, &format!("bin/{cmd}"));
        self.append(
            "state/manifest",
            &format!(
                "{name}|github:repo|{cmd}|{}\n",
                self.path(&format!("share/{name}")).display()
            ),
        );
    }

    fn append(&self, rel: &str, content: &str) {
        let path = self.path(rel);
        let mut existing = fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(content);
        self.write(rel, &existing);
    }

    /// Path -> (is_symlink, len, mtime) for everything under the fixture.
    fn snapshot(&self) -> BTreeMap<PathBuf, (bool, u64, std::time::SystemTime)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.dir.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path.clone());
                }
                out.insert(
                    path,
                    (
                        meta.file_type().is_symlink(),
                        meta.len(),
                        meta.modified().unwrap(),
                    ),
                );
            }
        }
        out
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Parses rows, asserting every row has exactly the five contract columns.
fn rows(output: &Output) -> Vec<Vec<String>> {
    text(&output.stdout)
        .lines()
        .map(|line| {
            let columns = line.split('\t').map(ToOwned::to_owned).collect::<Vec<_>>();
            assert_eq!(columns.len(), 5, "row must have five columns: {line:?}");
            assert!(
                columns[0] == "fail" || columns[0] == "warn",
                "bad severity: {line:?}"
            );
            assert!(!columns[4].is_empty(), "detail must not be empty: {line:?}");
            columns
        })
        .collect()
}

/// Returns (severity, package, kind, path) for compact assertions.
fn keys(output: &Output) -> Vec<(String, String, String, String)> {
    rows(output)
        .into_iter()
        .map(|row| {
            (
                row[0].clone(),
                row[1].clone(),
                row[2].clone(),
                row[3].clone(),
            )
        })
        .collect()
}

fn key(severity: &str, package: &str, kind: &str, path: &Path) -> (String, String, String, String) {
    (
        severity.to_owned(),
        package.to_owned(),
        kind.to_owned(),
        path.display().to_string(),
    )
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={:?} stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn healthy_install_prints_nothing_and_exits_zero() {
    let fixture = Fixture::new("healthy");
    fixture.healthy_release("owner/tool", "tool");
    fixture.healthy_repo("owner/kit", "kit");
    fixture.append("conf/deps.conf", "jq pkg\n");
    // Raw single-file release: no root, the public path is the binary.
    fixture.append("conf/deps.conf", "owner/raw github:release raw\n");
    let raw = fixture.write_executable("bin/raw");
    fixture.append(
        "state/manifest",
        &format!("owner/raw|github:release|raw|{}\n", raw.display()),
    );

    let output = fixture.health();

    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn health_reads_state_without_writing_or_locking() {
    let fixture = Fixture::new("read-only");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    fixture.write("state/.deferred-posts", "owner/tool|sudo-no-terminal|\n");
    let before = fixture.snapshot();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(fixture.snapshot(), before, "health must not write anything");
    assert!(
        !fixture.path("state/.lock").exists(),
        "health must not take the state lock"
    );
}

#[test]
fn dangling_extras_link_is_reported_with_remediation() {
    // The live incident: an archive release became a single binary, the old
    // root is gone, and its man/completion links still dangle in the ledger.
    let fixture = Fixture::new("dangling-extras");
    fixture.append("conf/deps.conf", "jdx/mise github:release mise\n");
    let public = fixture.write_executable("bin/mise");
    fixture.append(
        "state/manifest",
        &format!("jdx/mise|github:release|mise|{}\n", public.display()),
    );
    let link = fixture.link(
        &fixture.path("share/jdx/mise/man/man1/mise.1"),
        "share/man/man1/mise.1",
    );
    fixture.write("state/jdx/mise.links", &format!("{}\n", link.display()));

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "jdx/mise", "dangling-link", &link)]
    );
    assert!(rows(&output)[0][4].contains("remove the stale link"));
}

#[test]
fn absent_tracked_extra_is_not_a_problem() {
    let fixture = Fixture::new("absent-extra");
    fixture.healthy_release("owner/tool", "tool");
    fixture.append(
        "state/owner/tool.links",
        &format!("{}\n", fixture.path("share/man/man1/deleted.1").display()),
    );

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn public_command_problems_follow_the_dep_links_contract() {
    let fixture = Fixture::new("public-links");
    fixture.append("conf/deps.conf", "owner/kit github:repo\n");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/kit|github:repo|kit|{}\n",
            fixture.path("share/owner/kit").display()
        ),
    );
    fixture.write_executable("share/owner/kit/bin/kit-missing");
    fixture.write_executable("share/owner/kit/bin/kit-drift");
    fixture.write_executable("share/owner/kit/bin/kit-relative");
    fixture.write_executable("share/owner/kit/bin/kit-dangling");
    fixture.write_executable("share/owner/kit/bin/kit-mode");
    let elsewhere = fixture.write_executable("elsewhere/kit-drift");
    fixture.write_executable("elsewhere/kit-relative");
    let drift = fixture.link(&elsewhere, "bin/kit-drift");
    let relative = fixture.link(Path::new("../elsewhere/kit-relative"), "bin/kit-relative");
    let dangling = fixture.link(&fixture.path("gone/kit-dangling"), "bin/kit-dangling");
    // A link that resolves to the right file which then lost its execute bit.
    let mode = fixture.link(
        &fixture.path("share/owner/kit/bin/kit-mode"),
        "bin/kit-mode",
    );
    // A client's executable launcher in front of a command is preserved by
    // shdeps and is therefore not a problem.
    fixture.write_executable("share/owner/kit/bin/kit-adapter");
    fixture.write_executable("bin/kit-adapter");
    // Raw release whose public binary lost its execute bit.
    fixture.append("conf/deps.conf", "owner/raw github:release raw\n");
    let raw = fixture.write("bin/raw", "#!/bin/sh\n");
    fixture.append(
        "state/manifest",
        &format!("owner/raw|github:release|raw|{}\n", raw.display()),
    );

    let mode_target = fixture.path("share/owner/kit/bin/kit-mode");
    // dep-links lists only executables, so list it first, then drop the bit.
    let before = fixture.health();
    fs::set_permissions(&mode_target, fs::Permissions::from_mode(0o644)).unwrap();
    let output = fixture.health();

    assert_exit(&before, 1);
    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key(
                "warn",
                "owner/kit",
                "missing-binlink",
                &fixture.path("bin/kit-missing")
            ),
            key("warn", "owner/kit", "dangling-binlink", &dangling),
            key("warn", "owner/kit", "wrong-target", &drift),
            key("warn", "owner/kit", "wrong-target", &relative),
            key("fail", "owner/raw", "not-executable", &raw),
        ]
    );
    let _ = mode;
}

#[test]
fn not_executable_through_a_correct_link_is_reported() {
    let fixture = Fixture::new("not-executable-link");
    fixture.healthy_release("owner/tool", "tool");
    fs::set_permissions(
        fixture.path("share/owner/tool/bin/tool"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "fail",
            "owner/tool",
            "not-executable",
            &fixture.path("bin/tool")
        )]
    );
}

#[test]
fn links_through_symlinked_development_roots_are_healthy() {
    // A development clone in SHDEPS_GIT_DEV_DIR wins over the managed root,
    // which is itself a link to it; public links may name either spelling,
    // absolutely or relatively.
    let fixture = Fixture::new("dev-root");
    fixture.append("conf/deps.conf", "owner/kit github:repo\n");
    fixture.write_executable("git/kit/bin/kit-a");
    fixture.write_executable("git/kit/bin/kit-b");
    fixture.write_executable("git/kit/bin/kit-c");
    fixture.link(&fixture.path("git/kit"), "share/owner/kit");
    fixture.link(&fixture.path("share/owner/kit/bin/kit-a"), "bin/kit-a");
    fixture.link(&fixture.path("git/kit/bin/kit-b"), "bin/kit-b");
    fixture.link(Path::new("../share/owner/kit/bin/kit-c"), "bin/kit-c");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/kit|github:repo|kit|{}\n",
            fixture.path("share/owner/kit").display()
        ),
    );

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn dangling_tracked_binlink_is_reported_once() {
    let fixture = Fixture::new("dangling-binlink");
    fixture.healthy_release("owner/tool", "tool");
    fs::remove_file(fixture.path("share/owner/tool/bin/tool")).unwrap();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "warn",
            "owner/tool",
            "dangling-binlink",
            &fixture.path("bin/tool")
        )]
    );
}

#[test]
fn symlinked_release_root_from_another_installer_is_unmanaged() {
    // Shape of the standalone-installer incident: the stable root is a link
    // to `.tool-standalone/current`, which shdeps refuses to upgrade.
    let fixture = Fixture::new("symlinked-root");
    fixture.append("conf/deps.conf", "owner/tool github:release tool\n");
    let release = fixture.write_executable("share/owner/.tool-standalone/releases/v1/tool");
    let _ = release;
    fixture.link(
        Path::new("releases/v1"),
        "share/owner/.tool-standalone/current",
    );
    let root = fixture.link(Path::new(".tool-standalone/current"), "share/owner/tool");
    let public = fixture.link(&root.join("tool"), "bin/tool");
    fixture.append(
        "state/manifest",
        &format!("owner/tool|github:release|tool|{}\n", public.display()),
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "install-root-unmanaged", &root)]
    );
    assert!(rows(&output)[0][4].contains("symlink to .tool-standalone/current"));
}

#[test]
fn release_root_without_marker_or_proof_is_unmanaged() {
    let fixture = Fixture::new("unproven-root");
    fixture.append("conf/deps.conf", "owner/tool github:release tool\n");
    fixture.write_executable("share/owner/tool/bin/tool");
    // A regular public launcher beside an unmarked root is ambiguous.
    let public = fixture.write_executable("bin/tool");
    fixture.append(
        "state/manifest",
        &format!("owner/tool|github:release|tool|{}\n", public.display()),
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "fail",
            "owner/tool",
            "install-root-unmanaged",
            &fixture.path("share/owner/tool")
        )]
    );
    assert!(rows(&output)[0][4].contains(".shdeps-release-layout"));
}

#[test]
fn corrupt_release_marker_is_unmanaged() {
    let fixture = Fixture::new("corrupt-marker");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("share/owner/tool/.shdeps-release-layout", "v9 unknown\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "fail",
            "owner/tool",
            "install-root-unmanaged",
            &fixture.path("share/owner/tool")
        )]
    );
}

#[test]
fn legacy_root_proven_by_its_public_link_is_healthy() {
    // Pre-marker archives are proven by a live link into the root; update
    // backfills the marker, so health must not cry wolf.
    let fixture = Fixture::new("legacy-root");
    fixture.healthy_release("owner/tool", "tool");
    fs::remove_file(fixture.path("share/owner/tool/.shdeps-release-layout")).unwrap();

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn interrupted_archive_swap_backup_is_reported() {
    let fixture = Fixture::new("archive-backup");
    fixture.healthy_release("owner/tool", "tool");
    let backup = fixture.write("share/owner/tool.shdeps-archive-backup-42-7/bin/tool", "");
    let backup = backup.parent().unwrap().parent().unwrap().to_path_buf();
    // Unrelated siblings never match.
    fixture.write("share/owner/toolkit.shdeps-archive-backup-1-1/x", "");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "owner/tool", "archive-backup", &backup)]
    );
}

#[test]
fn deferred_hooks_are_reported_per_package() {
    let fixture = Fixture::new("deferred");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write(
        "state/.deferred-posts",
        "a/one|sudo-no-terminal|\nowner/tool|sudo-no-terminal|\n",
    );
    fixture.write(
        "state/.deferred-uninstalls",
        "old/dep|sudo-no-terminal|exit 1\nold/two|sudo-no-terminal|\n",
    );
    // A deferred post is pending too; only the deferral row is shown.
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    fixture.write("state/.pending-posts/a/one", "pending\n");
    // Prune keeps its journal for a deferred uninstall; no extra row.
    fixture.write("state/.prune-hooks-v1/abc.json", "{}\n");
    let posts = fixture.path("state/.deferred-posts");
    let uninstalls = fixture.path("state/.deferred-uninstalls");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key("warn", "a/one", "deferred-post", &posts),
            key("warn", "old/dep", "deferred-uninstall", &uninstalls),
            key("warn", "old/two", "deferred-uninstall", &uninstalls),
            key("warn", "owner/tool", "deferred-post", &posts),
        ]
    );
}

#[test]
fn pending_post_and_recovery_records_are_reported() {
    let fixture = Fixture::new("recovery");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    fixture.write("state/.method-transitions-v1/abc.json", "{}\n");
    fixture.write("state/.repo-publications/6f", "{}\n");
    fixture.write("state/.prune-hooks-v1/abc.json", "{}\n");
    fixture.write("state/owner/tool.links.reconcile-v1", "{}\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key(
                "warn",
                "-",
                "recovery-state",
                &fixture.path("state/.method-transitions-v1")
            ),
            key(
                "warn",
                "-",
                "recovery-state",
                &fixture.path("state/.prune-hooks-v1")
            ),
            key(
                "warn",
                "-",
                "recovery-state",
                &fixture.path("state/.repo-publications")
            ),
            key(
                "warn",
                "owner/tool",
                "pending-post",
                &fixture.path("state/.pending-posts/owner/tool")
            ),
            key(
                "warn",
                "owner/tool",
                "recovery-state",
                &fixture.path("state/owner/tool.links.reconcile-v1")
            ),
        ]
    );
}

#[test]
fn empty_recovery_directories_are_healthy() {
    let fixture = Fixture::new("empty-recovery");
    fixture.healthy_release("owner/tool", "tool");
    fs::create_dir_all(fixture.path("state/.prune-hooks-v1")).unwrap();
    fs::create_dir_all(fixture.path("state/.method-transitions-v1")).unwrap();

    assert_exit(&fixture.health(), 0);
}

#[test]
fn transient_records_are_suppressed_while_an_update_holds_the_lock() {
    let fixture = Fixture::new("update-running");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    fixture.write("state/.method-transitions-v1/abc.json", "{}\n");
    // A swap still deleting the old tree keeps its backup until it finishes.
    fixture.write("share/owner/tool.shdeps-archive-backup-1-2/bin/tool", "");
    let mut holder = Command::new("sleep").arg("30").spawn().unwrap();
    fixture.write(
        "state/.lock",
        &format!(
            "pid={}\nstate_dir=x\nacquired_unix={}\n",
            holder.id(),
            now_unix()
        ),
    );

    let running = fixture.health();
    holder.kill().unwrap();
    holder.wait().unwrap();
    let finished = fixture.health();

    assert_exit(&running, 0);
    assert_exit(&finished, 1);
    let kinds = rows(&finished)
        .into_iter()
        .map(|row| row[2].clone())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["recovery-state", "archive-backup", "pending-post"]);
}

#[test]
fn configured_but_not_installed_dependency_is_reported() {
    let fixture = Fixture::new("not-installed");
    fixture.append(
        "conf/deps.conf",
        "owner/kit github:repo\nowner/rel github:release rel\n",
    );
    fixture.append("conf/deps.conf", "owner/mac github:repo - - os:macos\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    let keys = keys(&output);
    assert_eq!(
        keys.iter()
            .map(|k| (k.1.as_str(), k.2.as_str()))
            .collect::<Vec<_>>(),
        [
            ("owner/kit", "not-installed"),
            ("owner/rel", "not-installed")
        ]
    );
}

#[test]
fn unreadable_state_exits_three_with_a_fail_row() {
    let fixture = Fixture::new("unreadable");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.deferred-posts", "owner/tool|sudo-no-terminal|\n");
    let manifest = fixture.path("state/manifest");
    fs::remove_file(&manifest).unwrap();
    fs::create_dir_all(&manifest).unwrap();

    let output = fixture.health();

    // Other problems are still reported alongside the unreadable state, but
    // no install is guessed missing without the manifest.
    assert_exit(&output, 3);
    assert_eq!(
        keys(&output),
        [
            key("fail", "-", "unreadable-state", &manifest),
            key(
                "warn",
                "owner/tool",
                "deferred-post",
                &fixture.path("state/.deferred-posts")
            ),
        ]
    );
}

#[test]
fn fifo_in_place_of_a_ledger_is_unreadable_not_a_hang() {
    let fixture = Fixture::new("fifo-ledger");
    fixture.healthy_release("owner/tool", "tool");
    let ledger = fixture.path("state/owner/tool.links");
    fs::remove_file(&ledger).unwrap();
    let status = Command::new("mkfifo").arg(&ledger).status().unwrap();
    assert!(status.success());

    let mut child = fixture
        .command(&["health"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("health blocked on a FIFO ledger");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();

    assert_eq!(status.code(), Some(3));
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "unreadable-state", &ledger)]
    );
}

#[test]
fn unwritable_output_exits_three() {
    let fixture = Fixture::new("unwritable-output");
    fixture.write("state/.deferred-posts", "owner/tool|sudo-no-terminal|\n");
    let full = fs::OpenOptions::new().write(true).open("/dev/full");
    let Ok(full) = full else {
        return; // no /dev/full on this platform
    };

    let output = fixture.command(&["health"]).stdout(full).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert!(text(&output.stderr).contains("cannot write health report"));
}

#[test]
fn health_honors_config_override() {
    let fixture = Fixture::new("config-override");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("other/deps.conf", "owner/absent github:repo\n");

    let default = fixture.health();
    let overridden = fixture
        .command(&["-c", fixture.path("other").to_str().unwrap(), "health"])
        .output()
        .unwrap();

    assert_exit(&default, 0);
    assert_exit(&overridden, 1);
    assert_eq!(rows(&overridden)[0][1], "owner/absent");
}

#[test]
fn missing_state_and_config_are_healthy() {
    let fixture = Fixture::new("fresh-host");
    fs::remove_dir_all(fixture.path("state")).unwrap();

    let output = fixture.health();

    assert_exit(&output, 0);
    assert!(
        !fixture.path("state").exists(),
        "health must not create state"
    );
}

#[test]
fn health_rejects_arguments_with_usage_status() {
    let fixture = Fixture::new("usage");

    let output = fixture.command(&["health", "extra"]).output().unwrap();

    assert_exit(&output, 2);
    assert_eq!(text(&output.stdout), "");
}

#[test]
fn toolchain_dependency_links_and_absence_are_checked() {
    let fixture = Fixture::new("toolchain");
    fixture.append("conf/deps.conf", "ripgrep cargo rg\nruff uv\n");
    let binary = fixture.write_executable("share/ripgrep/bin/rg");
    let public = fixture.link(&binary, "bin/rg");
    fixture.write("state/ripgrep.binlinks", &format!("{}\n", public.display()));
    fixture.append(
        "state/manifest",
        &format!(
            "ripgrep|cargo|rg|{}\n",
            fixture.path("share/ripgrep").display()
        ),
    );

    let healthy_rg = fixture.health();
    fs::remove_file(&binary).unwrap();
    let broken_rg = fixture.health();

    assert_eq!(
        keys(&healthy_rg),
        [key("warn", "ruff", "not-installed", Path::new("-"))]
    );
    assert_eq!(
        keys(&broken_rg),
        [
            key("warn", "ripgrep", "dangling-binlink", &public),
            key("warn", "ruff", "not-installed", Path::new("-")),
        ]
    );
}

#[test]
fn bare_github_needs_a_manifest_row_not_just_a_dev_clone() {
    // Update may still choose a release for a bare `github` entry, so a
    // development clone alone is not an install.
    let fixture = Fixture::new("bare-github");
    fixture.append("conf/deps.conf", "owner/tool github\n");
    fixture.write_executable("git/tool/bin/tool");

    let unrecorded = fixture.health();
    let row = rows(&unrecorded);

    assert_exit(&unrecorded, 1);
    assert_eq!(row.len(), 1);
    assert_eq!(row[0][1..3], ["owner/tool", "not-installed"]);
    assert!(row[0][4].contains("configured github dependency"));

    fixture.link(&fixture.path("git/tool/bin/tool"), "bin/tool");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.path("share/owner/tool").display()
        ),
    );
    assert_exit(&fixture.health(), 0);
}

#[test]
fn recorded_repo_without_a_checkout_is_not_installed() {
    let fixture = Fixture::new("repo-gone");
    fixture.append("conf/deps.conf", "owner/kit github:repo\n");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/kit|github:repo|kit|{}\n",
            fixture.path("share/owner/kit").display()
        ),
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "warn",
            "owner/kit",
            "not-installed",
            &fixture.path("share/owner/kit")
        )]
    );
}

#[test]
fn missing_archive_root_behind_a_public_link_is_unmanaged() {
    let fixture = Fixture::new("missing-root");
    fixture.healthy_release("owner/tool", "tool");
    fs::remove_dir_all(fixture.path("share/owner/tool")).unwrap();
    // Keep the extras ledger out of the picture.
    fs::remove_file(fixture.path("state/owner/tool.links")).unwrap();
    fs::remove_file(fixture.path("share/man/man1/tool.1")).unwrap();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key(
                "warn",
                "owner/tool",
                "dangling-binlink",
                &fixture.path("bin/tool")
            ),
            key(
                "fail",
                "owner/tool",
                "install-root-unmanaged",
                &fixture.path("share/owner/tool")
            ),
        ]
    );
}

#[test]
fn orphaned_dependency_ledgers_are_still_checked() {
    // Removed from config but not yet pruned: its links still dangle.
    let fixture = Fixture::new("orphan-ledger");
    fixture.append(
        "state/manifest",
        &format!(
            "old/tool|github:release|tool|{}\n",
            fixture.path("bin/tool").display()
        ),
    );
    let link = fixture.link(
        &fixture.path("share/old/tool/tool.fish"),
        "share/fish/tool.fish",
    );
    fixture.write("state/old/tool.links", &format!("{}\n", link.display()));

    let output = fixture.health();

    assert_eq!(
        keys(&output),
        [key("warn", "old/tool", "dangling-link", &link)]
    );
}

#[test]
fn reused_lock_owner_pid_does_not_suppress_records() {
    if !cfg!(target_os = "linux") {
        return; // the start-time check is Linux-only
    }
    let fixture = Fixture::new("reused-pid");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    let mut holder = Command::new("sleep").arg("30").spawn().unwrap();
    // The live process started long after this acquisition, so it cannot be
    // the owner that recorded it.
    fixture.write(
        "state/.lock",
        &format!("pid={}\nstate_dir=x\nacquired_unix=1000\n", holder.id()),
    );

    let output = fixture.health();
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_exit(&output, 1);
    assert_eq!(rows(&output)[0][2], "pending-post");
}

#[test]
fn health_help_is_not_an_unsupported_status() {
    let fixture = Fixture::new("help");

    let output = fixture.command(&["health", "--help"]).output().unwrap();

    assert_exit(&output, 0);
    assert!(text(&output.stdout).contains("health"));
}

#[test]
fn health_contract_is_advertised_and_tokens_are_stable() {
    assert!(
        PUBLIC_COMMANDS
            .iter()
            .any(|command| command.name == "health")
    );
    assert!(
        HELP.contains("  health                 Report problems with installed dependencies\n")
    );
    assert!(HELP.contains("<severity> <package> <kind> <path> <detail>"));
    assert!(HELP.contains("  3  health: report incomplete"));
    assert!(HELP.contains("New kinds may be added; ignore unknown kinds."));
    // Base dotfiles renders these tokens; renaming one is a breaking change.
    assert_eq!(
        ProblemKind::ALL.map(|kind| (kind.token(), kind.severity().token())),
        [
            ("missing-binlink", "warn"),
            ("dangling-binlink", "warn"),
            ("wrong-target", "warn"),
            ("not-executable", "fail"),
            ("dangling-link", "warn"),
            ("not-installed", "warn"),
            ("install-root-unmanaged", "fail"),
            ("archive-backup", "warn"),
            ("deferred-post", "warn"),
            ("deferred-uninstall", "warn"),
            ("pending-post", "warn"),
            ("recovery-state", "warn"),
            ("unreadable-state", "fail"),
        ]
    );
    for doc in ["README.md", "man/man1/shdeps.1"] {
        let content = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(doc)).unwrap();
        for kind in ProblemKind::ALL {
            assert!(
                content.contains(kind.token()),
                "{doc} must document {}",
                kind.token()
            );
        }
    }
}
