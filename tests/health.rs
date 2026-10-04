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

    /// Makes `apt` the detected package manager on every host. Each manager
    /// tool logs and fails if run, so tests can prove health queried none.
    fn fake_apt(&self) {
        for tool in ["apt-get", "apt-cache", "dpkg", "dpkg-query"] {
            let path = self.write(
                &format!("fakebin/{tool}"),
                &format!(
                    "#!/bin/sh\necho {tool} >> '{}'\nexit 99\n",
                    self.path("pkg-calls").display()
                ),
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Writes the package-check cache a clean `apt` update leaves, recording
    /// each command's PATH lookup (an empty path: proven without a command).
    fn pkg_cache(&self, pkg_mgr: &str, commands: &[(&str, &str)]) {
        // The runtime identity matches the fixture's test platform and host.
        let mut content = format!(
            "version\tshdeps-pkg-check-cache-v5\npkg_mgr\t{pkg_mgr}\n\
             platform\tlinux\nandroid\t0\nhost\ttest-host\n"
        );
        for (command, path) in commands {
            content.push_str(&format!("cmd\t{command}\t{path}\n"));
        }
        self.write("state/pkg-check-cache-v3", &content);
    }

    fn assert_no_pkg_calls(&self) {
        assert!(
            !self.path("pkg-calls").exists(),
            "health ran a package manager: {:?}",
            fs::read_to_string(self.path("pkg-calls"))
        );
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
    private_transition_dir(&fixture);

    assert_exit(&fixture.health(), 0);
}

#[test]
fn transient_records_are_suppressed_while_an_update_holds_the_lock() {
    let fixture = Fixture::new("update-running");
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/.pending-posts/owner/tool", "pending\n");
    fixture.write("state/.repo-publications/6f", "{}\n");
    // A swap still deleting the old tree keeps its backup until it finishes,
    // and a clone lives in its temp tree until it is published.
    fixture.write("share/owner/tool.shdeps-archive-backup-1-2/bin/tool", "");
    fixture.write("share/owner/tool.tmp.4242/.git/HEAD", "");
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
    assert_eq!(
        kinds,
        [
            "recovery-state",
            "archive-backup",
            "pending-post",
            "temp-tree"
        ]
    );
}

/// Creates the method-transition directory with the private mode Shdeps uses.
fn private_transition_dir(fixture: &Fixture) -> PathBuf {
    let dir = fixture.path("state/.method-transitions-v1");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Writes a private file the way Shdeps writes transition records.
fn write_private(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// The record file name Shdeps derives from a dependency name.
fn transition_stem(name: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(name.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn unreadable_method_transition_record_blocks_updates_and_names_its_package() {
    // Recovery refuses a record it cannot parse before acting on any other,
    // so every update and prune aborts: a fail naming the package and record.
    let fixture = Fixture::new("blocked-record");
    fixture.healthy_release("owner/tool", "tool");
    let record =
        private_transition_dir(&fixture).join(format!("{}.json", transition_stem("owner/tool")));
    write_private(&record, "{}\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &record)]
    );
    let detail = &rows(&output)[0][4];
    assert!(
        detail.contains("malformed method transition record"),
        "{detail}"
    );
    assert!(
        detail.ends_with("'shdeps update' fails until this is resolved"),
        "{detail}"
    );
}

#[test]
fn blocked_transitions_are_reported_while_an_update_holds_the_lock() {
    // A live update fails on the same record, so it is not work in progress.
    let fixture = Fixture::new("blocked-under-lock");
    fixture.healthy_release("owner/tool", "tool");
    let record = private_transition_dir(&fixture).join("unknown.json");
    write_private(&record, "not json\n");
    let mut holder = Command::new("sleep").arg("30").spawn().unwrap();
    fixture.write(
        "state/.lock",
        &format!(
            "pid={}\nstate_dir=x\nacquired_unix={}\n",
            holder.id(),
            now_unix()
        ),
    );

    let output = fixture.health();
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "-", "blocked-transition", &record)]
    );
}

#[test]
fn unsafe_method_transition_directory_blocks_even_when_empty() {
    // Recovery validates the directory's mode before listing records.
    let fixture = Fixture::new("unsafe-transition-dir");
    fixture.healthy_release("owner/tool", "tool");
    let dir = fixture.path("state/.method-transitions-v1");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "-", "blocked-transition", &dir)]
    );
    // The path column already names the directory; the detail does not
    // repeat it.
    assert_eq!(
        rows(&output)[0][4],
        "method transition state has unsafe ownership or mode; 'shdeps update' fails until this is resolved"
    );
}

#[test]
fn unexpected_method_transition_entries_block_updates() {
    let fixture = Fixture::new("unexpected-transition-entry");
    fixture.healthy_release("owner/tool", "tool");
    let dir = private_transition_dir(&fixture);
    let stray = dir.join("notes.txt");
    write_private(&stray, "x\n");
    let unindexed = dir.join(format!("{}.manifest", transition_stem("owner/tool")));
    write_private(&unindexed, "owner/tool|github:release|tool|-\n");
    // Staging temps are deleted by recovery and are not a problem.
    write_private(
        &dir.join(format!(".{}.json.tmp.12.34", transition_stem("owner/tool"))),
        "{}",
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key("fail", "-", "blocked-transition", &stray),
            key("fail", "owner/tool", "blocked-transition", &unindexed),
        ]
    );
    let details = rows(&output)
        .into_iter()
        .map(|row| row[4].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        details,
        [
            "unexpected entry in method transition state; 'shdeps update' fails until this is resolved",
            "unindexed prepared method-transition manifest exists; 'shdeps update' fails until this is resolved",
        ]
    );
}

#[test]
fn pending_repo_journal_is_recovery_state_for_its_package() {
    // A record write interrupted before the checkout moved: the next update
    // clears it by itself.
    let fixture = Fixture::new("repo-journal");
    fixture.healthy_repo("owner/tool", "tool");
    let journal = fixture.path("share/owner/.tool.shdeps-repo-transition-v1");
    fixture.write(
        "share/owner/.tool.shdeps-repo-transition-v1/.record.tmp.1.2.ab",
        "partial",
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "owner/tool", "recovery-state", &journal)]
    );
}

#[test]
fn malformed_repo_journal_blocks_its_package() {
    let fixture = Fixture::new("repo-journal-malformed");
    fixture.healthy_repo("owner/tool", "tool");
    let journal = fixture.path("share/owner/.tool.shdeps-repo-transition-v1");
    fixture.write("share/owner/.tool.shdeps-repo-transition-v1/record", "{}\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &journal)]
    );
    assert!(rows(&output)[0][4].contains("malformed repository transition record"));
}

#[test]
fn repo_journal_that_is_not_a_directory_names_it_once() {
    // The path column carries the journal; the detail keeps only the cause.
    let fixture = Fixture::new("repo-journal-file");
    fixture.healthy_repo("owner/tool", "tool");
    let journal = fixture.write("share/owner/.tool.shdeps-repo-transition-v1", "x\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &journal)]
    );
    let detail = &rows(&output)[0][4];
    assert!(
        detail.starts_with("repository transition is not a private directory (checkout "),
        "{detail}"
    );
    assert!(!detail.contains(&journal.display().to_string()), "{detail}");
}

#[test]
fn collided_repo_journal_blocks_its_package() {
    let fixture = Fixture::new("repo-journal-blocked");
    fixture.healthy_repo("owner/tool", "tool");
    let journal = fixture.path("share/owner/.tool.shdeps-repo-transition-v1");
    fixture.write("share/owner/.tool.shdeps-repo-transition-v1/record", "{}\n");
    fixture.write(
        "share/owner/.tool.shdeps-repo-transition-v1/blocked",
        "shdeps repository transition blocked v1\n",
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &journal)]
    );
    assert!(rows(&output)[0][4].contains("repository transition"));
}

#[test]
fn interrupted_collision_marker_also_blocks() {
    // Recovery treats a marker whose atomic write was interrupted as set.
    let fixture = Fixture::new("repo-journal-blocked-temp");
    fixture.healthy_repo("owner/tool", "tool");
    let journal = fixture.path("share/owner/.tool.shdeps-repo-transition-v1");
    fixture.write(
        "share/owner/.tool.shdeps-repo-transition-v1/.blocked.tmp.1.2.ab",
        "",
    );

    let output = fixture.health();

    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &journal)]
    );
}

#[test]
fn checkout_journals_are_read_at_the_physical_path_recovery_records() {
    // The checkout lock resolves the root's parent physically and the
    // journal records that spelling; through a symlinked owner directory a
    // valid, recoverable journal must not read as foreign.
    use std::os::unix::fs::MetadataExt as _;

    let fixture = Fixture::new("physical-journal");
    let physical = fixture.path("elsewhere/owner");
    fs::create_dir_all(&physical).unwrap();
    fs::create_dir_all(fixture.path("share")).unwrap();
    fixture.link(&physical, "share/owner");
    fixture.append("conf/deps.conf", "owner/tool github:repo\n");
    let checkout = physical.join("tool");
    fixture.append(
        "state/manifest",
        &format!("owner/tool|github:repo|tool|{}\n", checkout.display()),
    );
    // A fresh publication interrupted after its journal, before the rename.
    let staged = physical.join("tool.tmp.4242");
    fs::create_dir_all(staged.join("bin")).unwrap();
    let metadata = fs::symlink_metadata(&staged).unwrap();
    let journal = physical.join(".tool.shdeps-repo-transition-v1");
    fs::create_dir_all(&journal).unwrap();
    let record = serde_json::json!({
        "format": "shdeps repository transition v1",
        "checkout": checkout,
        "desired": {
            "Directory": {
                "identity": {
                    "kind": "directory",
                    "device": metadata.dev(),
                    "inode": metadata.ino(),
                    "target": null
                },
                "staged": staged
            }
        },
        "ownership": {
            "name": "owner/tool",
            "method": "github:repo",
            "cmd": "tool",
            "install_path": checkout
        }
    });
    write_private(&journal.join("record"), &format!("{record}\n"));

    let output = fixture.health();

    let journal_rows = keys(&output)
        .into_iter()
        .filter(|row| row.2 == "recovery-state" || row.2 == "blocked-transition")
        .collect::<Vec<_>>();
    assert_eq!(
        journal_rows,
        [key("warn", "owner/tool", "recovery-state", &journal)]
    );
}

#[test]
fn checkout_installer_transaction_blocks_its_package() {
    let fixture = Fixture::new("installer-transaction");
    fixture.healthy_repo("owner/tool", "tool");
    let transaction = fixture.write("share/owner/.tool.install.transaction", "pending\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "fail",
            "owner/tool",
            "blocked-transition",
            &transaction
        )]
    );
    assert!(rows(&output)[0][4].contains("rerun it before Shdeps"));
}

#[test]
fn orphaned_repo_checkouts_are_checked_for_journals() {
    // Prune recovers the checkout of a dependency no longer configured.
    let fixture = Fixture::new("orphan-journal");
    fixture.healthy_release("owner/tool", "tool");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/gone|github:repo|gone|{}\n",
            fixture.path("share/owner/gone").display()
        ),
    );
    let transaction = fixture.write("share/owner/.gone.install.transaction", "pending\n");

    let output = fixture.health();

    assert_eq!(
        keys(&output),
        [key(
            "fail",
            "owner/gone",
            "blocked-transition",
            &transaction
        )]
    );
}

#[test]
fn interrupted_temp_trees_are_reported_for_cleanup() {
    let fixture = Fixture::new("temp-trees");
    fixture.healthy_release("owner/tool", "tool");
    let clone = fixture.path("share/owner/tool.tmp.4242");
    fs::create_dir_all(clone.join(".git")).unwrap();
    let staging = fixture.path("share/owner/.tool.tmp.77");
    fs::create_dir_all(&staging).unwrap();
    // Atomic-write temps carry a nonce; other names merely share the prefix.
    fs::create_dir_all(fixture.path("share/owner/.tool.tmp.77.1")).unwrap();
    fs::create_dir_all(fixture.path("share/owner/tool.tmp.old")).unwrap();
    fs::create_dir_all(fixture.path("share/owner/toolbox.tmp.1")).unwrap();

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [
            key("warn", "owner/tool", "temp-tree", &staging),
            key("warn", "owner/tool", "temp-tree", &clone),
        ]
    );
}

#[test]
fn temp_trees_a_checkout_journal_may_publish_are_not_reported() {
    // A fresh publication journals its `<root>.tmp.<pid>` clone as the tree
    // recovery renames into place; deleting it would lose the install.
    let fixture = Fixture::new("temp-tree-journaled");
    fixture.healthy_repo("owner/tool", "tool");
    fs::create_dir_all(fixture.path("share/owner/tool.tmp.4242/.git")).unwrap();
    let journal = fixture.path("share/owner/.tool.shdeps-repo-transition-v1");
    fixture.write(
        "share/owner/.tool.shdeps-repo-transition-v1/.record.tmp.1.2.ab",
        "partial",
    );

    let output = fixture.health();

    assert_eq!(
        keys(&output),
        [key("warn", "owner/tool", "recovery-state", &journal)]
    );
}

#[test]
fn unreadable_public_command_transition_blocks_the_command_owner() {
    let fixture = Fixture::new("public-transition");
    fixture.healthy_release("owner/tool", "tool");
    let journal = fixture.write("bin/.tool.shdeps-public-transition-v1", "not json\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("fail", "owner/tool", "blocked-transition", &journal)]
    );
    assert!(rows(&output)[0][4].contains("malformed public command transition record"));
}

#[test]
fn runtime_identity_starts_no_host_probe_processes() {
    // `dot doctor` runs health on every invocation; identity comes from
    // uname(2), not from `uname`/`hostname` children.
    let fixture = Fixture::new("no-probe-processes");
    fixture.healthy_release("owner/tool", "tool");
    let marker = fixture.path("probe-calls");
    for probe in ["uname", "hostname"] {
        let path = fixture.path(&format!("fakebin/{probe}"));
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho {probe} \"$@\" >> '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = fixture
        .command(&["health"])
        .env_remove("SHDEPS_TEST_PLATFORM")
        .env_remove("SHDEPS_TEST_HOST")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(
        !marker.exists(),
        "health ran host probes: {:?}",
        fs::read_to_string(&marker)
    );
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
fn pkg_with_explicit_command_missing_from_path_is_not_installed() {
    let fixture = Fixture::new("pkg-missing");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "ripgrep", "not-installed", Path::new("-"))]
    );
    let detail = &rows(&output)[0][4];
    assert!(detail.contains("'shdeps-test-rg'"), "{detail}");
    assert!(detail.contains("'shdeps update'"), "{detail}");
    fixture.assert_no_pkg_calls();
}

#[test]
fn pkg_update_found_unavailable_here_is_not_reported() {
    // `update` skips a package this manager cannot offer and exits 0, and
    // that one skip keeps it from writing the package-check cache. Health
    // must agree that the host is fine, yet still catch an available
    // package whose command is genuinely gone.
    let fixture = Fixture::new("pkg-unavailable");
    let log = fixture.path("pkg-calls");
    fixture.append(
        "conf/deps.conf",
        "eza pkg shdeps-test-eza\ngh pkg shdeps-test-gh\n",
    );
    let gh = fixture.write_executable("fakebin/shdeps-test-gh");
    fixture.write_executable("fakebin/id");
    fs::write(fixture.path("fakebin/id"), "#!/bin/sh\necho 0\n").unwrap();
    for (tool, body) in [
        ("apt-get", "exit 0"),
        // gh is installed; the batch listing (no package argument) has it.
        (
            "dpkg-query",
            "for last; do :; done\n\
             case $last in\n\
             gh) case $2 in *Package*) printf 'install ok installed\\tgh\\t1\\n' ;; \
             *) printf 'install ok installed\\n' ;; esac ;;\n\
             -f=*) printf 'install ok installed\\tgh\\t1\\n' ;;\n\
             *) exit 1 ;;\n\
             esac",
        ),
        // Only gh exists in this release's archive.
        ("apt-cache", "[ \"$1:$2\" = show:gh ]"),
    ] {
        fixture.write(
            &format!("fakebin/{tool}"),
            &format!(
                "#!/bin/sh\necho \"{tool} $*\" >> '{}'\n{body}\n",
                log.display()
            ),
        );
        fs::set_permissions(
            fixture.path(&format!("fakebin/{tool}")),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let update = fixture.command(&["-q", "update"]).output().unwrap();
    assert_exit(&update, 0);
    assert!(
        !fixture.path("state/pkg-check-cache-v3").exists(),
        "an unavailable package must keep the cache unwritten"
    );
    let record = fs::read_to_string(fixture.path("state/pkg-unavailable")).unwrap();
    assert!(record.ends_with("\neza\teza\n"), "{record}");

    // A quiet run without sudo skips before checking availability; it must
    // keep the verdict rather than make health flap until the next run.
    fs::write(fixture.path("fakebin/id"), "#!/bin/sh\necho 1000\n").unwrap();
    fixture.write("fakebin/sudo", "#!/bin/sh\nexit 1\n");
    fs::set_permissions(
        fixture.path("fakebin/sudo"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let quiet = fixture.command(&["-q", "update"]).output().unwrap();
    assert_exit(&quiet, 0);
    assert_eq!(
        fs::read_to_string(fixture.path("state/pkg-unavailable")).unwrap(),
        record
    );
    fs::remove_file(&log).unwrap();

    let output = fixture.health();
    assert_exit(&output, 0);

    fs::remove_file(&gh).unwrap();
    let output = fixture.health();
    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "gh", "not-installed", Path::new("-"))]
    );
    fixture.assert_no_pkg_calls();
}

#[test]
fn pkg_unavailable_record_for_another_package_does_not_suppress() {
    // The config now names a package the last scan never checked.
    let fixture = Fixture::new("pkg-unavailable-other");
    fixture.fake_apt();
    fixture.append(
        "conf/deps.conf",
        "eza pkg shdeps-test-eza apt:eza-new\nexa pkg shdeps-test-exa\n",
    );
    // `exa` still matches its record, which proves the record is read.
    fixture.write(
        "state/pkg-unavailable",
        "version\tshdeps-pkg-unavailable-v1\npkg_mgr\tapt\nplatform\tlinux\n\
         android\t0\nhost\ttest-host\neza\teza\nexa\texa\n",
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "eza", "not-installed", Path::new("-"))]
    );
}

#[test]
fn pkg_command_on_path_is_healthy_whoever_provides_it() {
    // Update counts any executable on PATH as the package being present (a
    // release binary or a hook fallback may provide it); health agrees.
    let fixture = Fixture::new("pkg-present");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");
    fixture.write_executable("fakebin/shdeps-test-rg");

    let output = fixture.health();

    assert_exit(&output, 0);
    fixture.assert_no_pkg_calls();
}

#[test]
fn pkg_command_that_is_not_executable_counts_as_missing() {
    let fixture = Fixture::new("pkg-not-exec");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");
    fixture.write("fakebin/shdeps-test-rg", "#!/bin/sh\n");

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "ripgrep", "not-installed", Path::new("-"))]
    );
}

#[test]
fn pkg_with_default_command_is_not_guessed_missing() {
    // A defaulted command may not exist at all: completion data, fonts, and
    // libraries are proven by the package manager, which health never asks.
    let fixture = Fixture::new("pkg-default-cmd");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "shdeps-test-data pkg\n");
    fixture.append("conf/deps.conf", "shdeps-test-data2 pkg -\n");

    let output = fixture.health();

    assert_exit(&output, 0);
    fixture.assert_no_pkg_calls();
}

#[test]
fn pkg_command_the_last_clean_update_found_is_reported_when_gone() {
    let fixture = Fixture::new("pkg-cache-found");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "shdeps-test-gh pkg\n");
    fixture.pkg_cache("apt", &[("shdeps-test-gh", "/usr/bin/shdeps-test-gh")]);

    let output = fixture.health();

    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key(
            "warn",
            "shdeps-test-gh",
            "not-installed",
            Path::new("-")
        )]
    );
    fixture.assert_no_pkg_calls();
}

#[test]
fn pkg_the_last_clean_update_proved_without_a_command_is_not_reported() {
    let fixture = Fixture::new("pkg-cache-commandless");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "python pkg shdeps-test-python\n");
    fixture.pkg_cache("apt", &[("shdeps-test-python", "")]);

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn pkg_cache_from_another_manager_is_not_evidence() {
    let fixture = Fixture::new("pkg-cache-other-mgr");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "shdeps-test-gh pkg\n");
    fixture.append("conf/deps.conf", "python pkg shdeps-test-python\n");
    fixture.pkg_cache(
        "dnf",
        &[
            ("shdeps-test-gh", "/usr/bin/shdeps-test-gh"),
            ("shdeps-test-python", ""),
        ],
    );

    let output = fixture.health();

    // Without usable evidence the defaulted command is skipped and the
    // declared one is still checked.
    assert_exit(&output, 1);
    assert_eq!(
        keys(&output),
        [key("warn", "python", "not-installed", Path::new("-"))]
    );
}

#[test]
fn pkg_excluded_on_this_host_is_not_reported() {
    let fixture = Fixture::new("pkg-filtered");
    fixture.fake_apt();
    fixture.append(
        "conf/deps.conf",
        "mac-only pkg shdeps-test-mac - os:macos\n\
         dnf-only pkg shdeps-test-dnf - mgr:dnf\n\
         no-apt-package pkg shdeps-test-none apt:NONE\n",
    );

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn pkg_manager_qualified_command_resolves_like_update() {
    let fixture = Fixture::new("pkg-qualified");
    fixture.fake_apt();
    // `apt:` picks the Debian command name; an unmatched qualifier falls
    // back to the defaulted name, which is not guessed missing.
    fixture.append(
        "conf/deps.conf",
        "bat pkg apt:shdeps-test-batcat\n\
         shdeps-test-fd pkg dnf:shdeps-test-fdfind\n",
    );

    let missing = fixture.health();
    assert_exit(&missing, 1);
    assert_eq!(
        keys(&missing),
        [key("warn", "bat", "not-installed", Path::new("-"))]
    );
    assert!(rows(&missing)[0][4].contains("'shdeps-test-batcat'"));

    fixture.write_executable("fakebin/shdeps-test-batcat");
    assert_exit(&fixture.health(), 0);
}

#[test]
fn pkg_android_qualified_command_resolves_on_termux() {
    let fixture = Fixture::new("pkg-termux");
    fixture.fake_apt();
    fixture.append(
        "conf/deps.conf",
        "fd pkg android:shdeps-test-fd,apt:shdeps-test-fdfind\n",
    );
    fixture.write_executable("fakebin/shdeps-test-fdfind");

    let output = fixture
        .command(&["health"])
        .env("TERMUX_VERSION", "0.118.3")
        .output()
        .unwrap();

    assert_exit(&output, 1);
    assert!(rows(&output)[0][4].contains("'shdeps-test-fd'"));
}

#[test]
fn pkg_git_subcommand_is_not_guessed_missing() {
    // Update also accepts `git <sub>` from Git's exec path, which only a
    // subprocess can see.
    let fixture = Fixture::new("pkg-git-sub");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "absorb pkg git-shdeps-test-absorb\n");

    let output = fixture.health();

    assert_exit(&output, 0);
}

#[test]
fn pkg_commands_are_not_checked_without_path() {
    // The PATH guard is only reachable once a manager is detected, and
    // detection itself walks PATH. An empty PATH is one empty element, which
    // lookups resolve against the working directory, so running from the
    // fake manager's directory detects apt while every command lookup
    // misses. Without the guard this reports `shdeps-test-rg` missing.
    let fixture = Fixture::new("pkg-no-path");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");

    let output = fixture
        .command(&["health"])
        .env("PATH", "")
        .current_dir(fixture.path("fakebin"))
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(
        !fixture.path("pkg-calls").exists(),
        "health must not run the package manager"
    );
}

#[test]
fn pkg_commands_are_not_checked_without_a_package_manager() {
    // Detection failed (no manager on this PATH), so filters and package
    // names cannot be resolved the way update would resolve them.
    let fixture = Fixture::new("pkg-no-mgr");
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");

    let output = fixture
        .command(&["health"])
        .env("PATH", fixture.path("fakebin"))
        .output()
        .unwrap();

    assert_exit(&output, 0);
}

#[test]
fn pkg_command_in_the_bin_dir_is_found_off_path() {
    // Update puts the bin dir first on PATH, so a command a hook or another
    // method linked there satisfies it even when the caller's PATH lacks it.
    let fixture = Fixture::new("pkg-bin-dir");
    fixture.fake_apt();
    fixture.append("conf/deps.conf", "ripgrep pkg shdeps-test-rg\n");
    fixture.write_executable("bin/shdeps-test-rg");

    let output = fixture.health();

    assert_exit(&output, 0);
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
fn checkout_stuck_behind_its_peers_is_a_stale_remote() {
    // The pull failure `update` only warns about once per run is durable
    // here: the record names the cause, peer stamps prove it is stuck.
    let fixture = Fixture::new("stale-remote");
    let now = now_unix();
    fixture.healthy_release("owner/tool", "tool");
    fixture.healthy_repo("owner/kit", "kit");
    fixture.write("state/owner/tool.release.stamp", &format!("{now}\n"));
    fixture.write(
        "state/owner/kit.repo.stamp",
        &format!("{}\n", now - 3 * 86_400),
    );
    fixture.write(
        "state/owner/kit.repo.pull-failure",
        &format!(
            "since={}\nlast={now}\nreason=fetch\ndetail=Could not resolve host: github.com\n",
            now - 3 * 86_400 + 60
        ),
    );

    let output = fixture.health();

    assert_exit(&output, 1);
    let root = fixture.path("share/owner/kit");
    assert_eq!(
        keys(&output),
        [key("warn", "owner/kit", "stale-remote", &root)]
    );
    assert_eq!(
        rows(&output)[0][4],
        format!(
            "checkout has not refreshed for 3d (fetch failed: Could not resolve host: github.com); check network and GitHub access with 'git -C {} fetch', then run 'shdeps update'",
            root.display()
        )
    );
}

#[test]
fn stale_remote_is_suppressed_while_an_update_is_stamping() {
    // After a long sleep a running update stamps sources one by one; the
    // ones it has not reached yet are not stuck.
    let fixture = Fixture::new("stale-remote-running");
    let now = now_unix();
    fixture.healthy_release("owner/tool", "tool");
    fixture.healthy_repo("owner/kit", "kit");
    fixture.write("state/owner/tool.release.stamp", &format!("{now}\n"));
    fixture.write(
        "state/owner/kit.repo.stamp",
        &format!("{}\n", now - 3 * 86_400),
    );
    let mut holder = Command::new("sleep").arg("30").spawn().unwrap();
    fixture.write(
        "state/.lock",
        &format!("pid={}\nstate_dir=x\nacquired_unix={now}\n", holder.id()),
    );

    let running = fixture.health();
    holder.kill().unwrap();
    holder.wait().unwrap();
    let finished = fixture.health();

    assert_exit(&running, 0);
    assert_exit(&finished, 1);
    assert_eq!(rows(&finished)[0][2], "stale-remote");
}

#[test]
fn development_clone_and_fresh_peers_are_not_stale_remotes() {
    // A development clone skips its pull while it has local edits, so its
    // stamp trailing the others is normal; a stamp within a day of its
    // newest peer is too.
    let fixture = Fixture::new("stale-remote-quiet");
    let now = now_unix();
    fixture.healthy_release("owner/tool", "tool");
    fixture.write("state/owner/tool.release.stamp", &format!("{now}\n"));
    fixture.healthy_repo("owner/kit", "kit");
    fixture.write("state/owner/kit.repo.stamp", &format!("{}\n", now - 86_400));
    fixture.append("conf/deps.conf", "owner/dev github:repo\n");
    let binary = fixture.write_executable("git/dev/bin/dev");
    fixture.link(&fixture.path("git/dev"), "share/owner/dev");
    fixture.link(&binary, "bin/dev");
    fixture.append(
        "state/manifest",
        &format!(
            "owner/dev|github:repo|dev|{}\n",
            fixture.path("share/owner/dev").display()
        ),
    );
    fixture.write(
        "state/owner/dev.repo.stamp",
        &format!("{}\n", now - 30 * 86_400),
    );

    let output = fixture.health();

    assert_exit(&output, 0);
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
            ("stale-remote", "warn"),
            ("blocked-transition", "fail"),
            ("temp-tree", "warn"),
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
