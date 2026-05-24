use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn shdeps() -> Command {
    Command::new(env!("CARGO_BIN_EXE_shdeps"))
}

#[test]
fn version_output_is_commit_based() {
    let output = run(shdeps().arg("version"));

    assert_success(&output);
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("shdeps commit "),
        "unexpected version output: {stdout:?}"
    );
    assert!(
        !stdout.contains("unknown"),
        "version must never fall back to unknown: {stdout:?}"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn help_output_is_stable_and_hides_removed_migrate_command() {
    let output = run(shdeps().arg("help"));

    assert_success(&output);
    let stdout = text(&output.stdout);
    assert!(stdout.starts_with("Usage: shdeps [options] <command> [args]\n"));
    assert!(stdout.contains("  dep-file <name> <rel>\n"));
    assert!(
        !stdout.contains("migrate"),
        "removed migrate command must stay out of user-facing help"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn usage_errors_match_public_cli_contract() {
    let unknown = run(shdeps().arg("--version"));
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(text(&unknown.stdout), "");
    assert_eq!(
        text(&unknown.stderr),
        "error: unknown option '--version'\nRun 'shdeps help' for usage.\n"
    );

    let missing_rel = run(shdeps().args(["dep-path", "cgraf78/sley"]));
    assert_eq!(missing_rel.status.code(), Some(2));
    assert_eq!(text(&missing_rel.stdout), "");
    assert_eq!(
        text(&missing_rel.stderr),
        "error: dep-path requires a dependency name and relative path\nUsage: shdeps dep-path <name> <relative-path>\n"
    );

    let migrate = run(shdeps().arg("migrate"));
    assert_eq!(migrate.status.code(), Some(2));
    assert_eq!(text(&migrate.stdout), "");
    assert_eq!(
        text(&migrate.stderr),
        "error: migrate has been removed from the user-facing CLI\nRun 'shdeps help' for usage.\n"
    );
}

#[test]
fn path_helpers_resolve_installed_assets_with_clean_stdout() {
    let fixture = Fixture::new("path-helpers");
    fixture.write("conf/deps.conf", "cgraf78/sley  github:repo\n");
    fixture.write("share/cgraf78/sley/share/sley/shell.sh", "SLEY=installed\n");

    let root = run(&mut fixture.command(["dep-root", "cgraf78/sley"]));
    assert_success(&root);
    assert_eq!(
        text(&root.stdout),
        format!(
            "{}\n",
            fixture
                .dir
                .join("share/cgraf78/sley")
                .canonicalize()
                .unwrap()
                .display()
        )
    );
    assert_eq!(text(&root.stderr), "");

    let file = run(&mut fixture.command(["dep-file", "cgraf78/sley", "share/sley/shell.sh"]));
    assert_success(&file);
    assert_eq!(
        text(&file.stdout),
        format!(
            "{}/share/sley/shell.sh\n",
            fixture
                .dir
                .join("share/cgraf78/sley")
                .canonicalize()
                .unwrap()
                .display()
        )
    );
    assert_eq!(text(&file.stderr), "");
}

#[test]
fn read_only_api_outputs_machine_clean_lines() {
    let fixture = Fixture::new("api");
    fixture.write("conf/deps.conf", "owner/tool.git github:repo\njq pkg\n");

    let version = run(&mut fixture.command(["__api", "version"]));
    assert_success(&version);
    assert_eq!(text(&version.stdout), "abi:1\n");
    assert_eq!(text(&version.stderr), "");

    let count = run(&mut fixture.command(["__api", "load-count"]));
    assert_success(&count);
    assert_eq!(text(&count.stdout), "2\n");
    assert_eq!(text(&count.stderr), "");

    let snapshot = run(&mut fixture.command(["--force", "__api", "env-snapshot"]));
    assert_success(&snapshot);
    let stdout = text(&snapshot.stdout);
    assert!(stdout.contains("install_dir="));
    assert!(stdout.contains("bin_dir="));
    assert!(stdout.contains("git_dev_dir="));
    assert!(stdout.contains("platform=linux\n"));
    assert!(stdout.contains("pkg_mgr=\n"));
    assert!(stdout.contains("force=1\n"));
    assert!(stdout.contains("reinstall=0\n"));
    assert!(stdout.contains("abi=1\n"));
    assert_eq!(text(&snapshot.stderr), "");
}

#[test]
fn mutating_api_links_and_unlinks_extras() {
    let fixture = Fixture::new("api-extras");
    let install = fixture.dir.join("share/owner/tool");
    let install_arg = install.to_string_lossy().into_owned();
    fixture.write("share/owner/tool/share/man/man1/tool.1", ".TH TOOL 1\n");

    let link =
        run(&mut fixture.command(["__api", "link-extras", "owner/tool", install_arg.as_str()]));

    assert_success(&link);
    assert_eq!(text(&link.stdout), "");
    assert_eq!(text(&link.stderr), "");
    assert_eq!(
        fs::read_link(fixture.dir.join("share/man/man1/tool.1")).unwrap(),
        install.join("share/man/man1/tool.1")
    );
    assert!(fixture.dir.join("state/owner/tool.links").exists());

    let unlink = run(&mut fixture.command(["__api", "unlink-extras", "owner/tool"]));

    assert_success(&unlink);
    assert_eq!(text(&unlink.stdout), "");
    assert_eq!(text(&unlink.stderr), "");
    assert!(!fixture.dir.join("share/man/man1/tool.1").exists());
    assert!(!fixture.dir.join("state/owner/tool.links").exists());
}

#[test]
fn dep_file_stays_fast_with_many_configured_dependencies() {
    let fixture = Fixture::new("dep-file-perf");
    let mut config = String::new();
    for index in 0..100 {
        config.push_str(&format!("owner/tool-{index:03}  github:repo\n"));
    }
    config.push_str("cgraf78/sley  github:repo\n");
    fixture.write("conf/deps.conf", &config);
    fixture.write("share/cgraf78/sley/share/sley/shell.sh", "SLEY=installed\n");

    // Warm once so this test measures the cheap command path rather than
    // one-time dynamic loader or filesystem cache noise from starting the test
    // binary for the first time. The command must still do real process startup
    // and config parsing, which is the user-visible cost for editor/shell use.
    assert_success(&run(&mut fixture.command([
        "dep-file",
        "cgraf78/sley",
        "share/sley/shell.sh",
    ])));

    let started = Instant::now();
    let output = run(&mut fixture.command(["dep-file", "cgraf78/sley", "share/sley/shell.sh"]));
    let elapsed = started.elapsed();

    assert_success(&output);
    assert!(
        elapsed <= Duration::from_millis(200),
        "dep-file should stay under the CI cheap-command budget; elapsed={elapsed:?}, stdout={:?}, stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn list_reports_configured_dependency_statuses() {
    let fixture = Fixture::new("list-status");
    fixture.write(
        "conf/deps.conf",
        "cgraf78/tool github:repo\nlinux-only github:repo - - os:mac\nasset github:release asset\ncustom custom\n",
    );
    fixture.write("share/cgraf78/tool/VERSION", "2.0.0\n");
    fixture.write_executable("bin/asset", "#!/bin/sh\n");
    fixture.write(
        "state/manifest",
        &format!(
            "asset|github:release|asset|{}\n",
            fixture.dir.join("bin/asset").display()
        ),
    );
    fixture.write(
        "conf/hooks.d/custom.sh",
        "exists() { return 0; }\nversion() { printf '9.9.9\\n'; }\n",
    );

    let output = run(&mut fixture.command(["list"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "NAME         METHOD         STATUS       DETAILS\n\
         ----         ------         ------       -------\n\
         asset        github:release installed    \n\
         cgraf78/tool github:repo    installed    2.0.0\n\
         custom       custom         installed    9.9.9\n\
         linux-only   github:repo    skipped      (platform)\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn check_reports_installed_skipped_missing_and_unknown() {
    let fixture = Fixture::new("check-status");
    fixture.write(
        "conf/deps.conf",
        "cgraf78/tool github:repo\nlinux-only github:repo - - os:mac\nmissing github:repo\n",
    );
    fixture.write("share/cgraf78/tool/VERSION", "2.0.0\n");

    let installed = run(&mut fixture.command(["check", "cgraf78/tool"]));
    assert_success(&installed);
    assert_eq!(text(&installed.stdout), "cgraf78/tool: installed (2.0.0)\n");
    assert_eq!(text(&installed.stderr), "");

    let skipped = run(&mut fixture.command(["check", "linux-only"]));
    assert_success(&skipped);
    assert_eq!(
        text(&skipped.stdout),
        "linux-only: skipped (platform mismatch)\n"
    );
    assert_eq!(text(&skipped.stderr), "");

    let missing = run(&mut fixture.command(["check", "missing"]));
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(text(&missing.stdout), "missing: not installed\n");
    assert_eq!(text(&missing.stderr), "");

    let unknown = run(&mut fixture.command(["check", "not-configured"]));
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(text(&unknown.stdout), "");
    assert_eq!(
        text(&unknown.stderr),
        "error: unknown dependency 'not-configured'\n"
    );

    let usage = run(&mut fixture.command(["check"]));
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(text(&usage.stdout), "");
    assert_eq!(
        text(&usage.stderr),
        "error: check requires a dependency name\nUsage: shdeps check <name>\n"
    );
}

#[test]
fn update_reports_empty_config_without_touching_installers() {
    let fixture = Fixture::new("update-empty");

    let output = run(&mut fixture.command(["update"]));

    assert_success(&output);
    assert_eq!(text(&output.stdout), "No dependencies configured.\n");
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_installs_custom_dep_runs_post_and_records_manifest() {
    let fixture = Fixture::new("update-custom");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  mkdir -p "$SHDEPS_STATE_DIR"
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
  printf '1.2.3\n'
}
post() { printf '%s:%s\n' "$1" "$SHDEPS_HOOK_PHASE" >"$SHDEPS_STATE_DIR/tool-post"; }
"#,
    );

    let first = run(&mut fixture.command(["update"]));

    assert_success(&first);
    assert_eq!(text(&first.stdout), "  tool: 1.2.3\n");
    assert_eq!(text(&first.stderr), "");
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/manifest")).unwrap(),
        "tool|custom|tool|\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/tool-post")).unwrap(),
        "tool:post\n"
    );

    let second = run(&mut fixture.command(["update"]));

    assert_success(&second);
    assert_eq!(text(&second.stdout), "");
    assert_eq!(text(&second.stderr), "");
}

#[test]
fn update_fails_when_custom_install_fails() {
    let fixture = Fixture::new("update-custom-fail");
    fixture.write("conf/deps.conf", "broken custom\n");
    fixture.write(
        "conf/hooks.d/broken.sh",
        "exists() { return 1; }\ninstall() { return 42; }\n",
    );

    let output = run(&mut fixture.command(["update"]));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "  broken failed: custom install failed\n"
    );
    assert!(!fixture.dir.join("state/manifest").exists());
}

#[test]
fn update_reports_orphans_without_pruning_them() {
    let fixture = Fixture::new("update-orphans");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "state/manifest",
        &format!(
            "old|github:release|old|{}\n",
            fixture.dir.join("bin/old").display()
        ),
    );
    fixture.write_executable("bin/old", "#!/bin/sh\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() { printf 'installed\n'; }
"#,
    );

    let output = run(&mut fixture.command(["update"]));

    assert_success(&output);
    assert_eq!(text(&output.stdout), "  tool: installed\n");
    assert_eq!(
        text(&output.stderr),
        "==> 1 orphaned dep(s) no longer in config:\n  old (github:release)\nRun `shdeps prune` to remove orphaned artifacts.\n"
    );
    assert!(fixture.dir.join("bin/old").exists());
    assert!(fs::read_to_string(fixture.dir.join("state/manifest"))
        .unwrap()
        .contains("old|github:release|old|"));
}

#[test]
fn prune_lists_dry_runs_and_removes_orphans() {
    let fixture = Fixture::new("prune");
    fixture.write("conf/deps.conf", "current github:repo\n");
    fixture.write(
        "state/manifest",
        "old|github:release|old|/tmp/old\ncurrent|github:repo|current|/tmp/current\n",
    );
    fixture.write_executable("bin/old", "#!/bin/sh\n");
    fixture.write("share/old/artifact", "artifact\n");
    fixture.write(
        "conf/hooks.d/old.sh",
        "uninstall() { printf '%s\\n' \"$1\" > \"$SHDEPS_STATE_DIR/hook-ran\"; }\n",
    );

    let dry = run(&mut fixture.command(["prune", "--dry-run"]));
    assert_success(&dry);
    assert_eq!(
        text(&dry.stdout),
        "==> 1 orphaned dep(s) no longer in config:\n  old (github:release)\nDry run — nothing removed.\n"
    );
    assert_eq!(text(&dry.stderr), "");
    assert!(fixture.dir.join("bin/old").exists());
    assert!(text(&fs::read(fixture.dir.join("state/manifest")).unwrap()).contains("old|"));

    let removed = run(&mut fixture.command(["prune", "-y"]));
    assert_success(&removed);
    assert_eq!(
        text(&removed.stdout),
        "==> 1 orphaned dep(s) no longer in config:\n  old (github:release)\n  old removed\n"
    );
    assert_eq!(text(&removed.stderr), "");
    assert!(!fixture.dir.join("bin/old").exists());
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/hook-ran")).unwrap(),
        "old\n"
    );
    assert!(!fs::read_to_string(fixture.dir.join("state/manifest"))
        .unwrap()
        .contains("old|"));
}

#[test]
fn prune_preserves_packages_and_guards_empty_config() {
    let fixture = Fixture::new("prune-pkg");
    fixture.write("state/manifest", "pkg-tool|pkg|pkg-tool|\n");

    let guarded = run(&mut fixture.command(["prune"]));
    assert_eq!(guarded.status.code(), Some(1));
    assert_eq!(text(&guarded.stdout), "");
    assert_eq!(
        text(&guarded.stderr),
        "warning: no deps in config but 1 in manifest — all would be orphaned\n  If intentional, re-run with -y\n"
    );
    assert!(fs::read_to_string(fixture.dir.join("state/manifest"))
        .unwrap()
        .contains("pkg-tool|"));

    let removed_tracking = run(&mut fixture.command(["prune", "-y"]));
    assert_success(&removed_tracking);
    assert_eq!(
        text(&removed_tracking.stdout),
        "==> 1 orphaned dep(s) no longer in config:\n  pkg-tool (pkg)\n"
    );
    assert_eq!(
        text(&removed_tracking.stderr),
        "  pkg-tool: pkg dep — remove manually via system package manager\n"
    );
    assert!(fs::read_to_string(fixture.dir.join("state/manifest"))
        .unwrap()
        .is_empty());
}

#[test]
fn self_update_reports_unsupported_non_checkout_installs() {
    let fixture = Fixture::new("self-update-unsupported");
    let install = fixture.dir.join("shdeps");
    fs::create_dir_all(&install).unwrap();
    let mut command = fixture.command(["self-update"]);
    command.env("SHDEPS_DIR", &install);

    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stdout).is_empty());
    assert!(text(&output.stderr).contains("no install metadata"));
}

fn run(command: &mut Command) -> Output {
    command.output().expect("shdeps command should run")
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={:?} stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("command output should be UTF-8")
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-cli-integration-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn command<const N: usize>(&self, args: [&str; N]) -> Command {
        let mut command = shdeps();
        command
            .env_clear()
            .env("HOME", self.dir.join("home"))
            .env("SHDEPS_CONF_DIR", self.dir.join("conf"))
            .env("SHDEPS_STATE_DIR", self.dir.join("state"))
            .env("SHDEPS_GIT_DEV_DIR", self.dir.join("git"))
            .env("SHDEPS_INSTALL_DIR", self.dir.join("share"))
            .env("SHDEPS_BIN_DIR", self.dir.join("bin"))
            .env("SHDEPS_TEST_PLATFORM", "linux")
            .env("SHDEPS_TEST_HOST", "test-host")
            .args(args);
        command
    }

    fn write(&self, rel: impl AsRef<Path>, content: &str) {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn write_executable(&self, rel: impl AsRef<Path>, content: &str) {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}
