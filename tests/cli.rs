use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn shdeps() -> Command {
    Command::new(env!("CARGO_BIN_EXE_shdeps"))
}

#[test]
fn version_output_is_generated_and_commit_traceable() {
    let output = run(shdeps().arg("version"));

    assert_success(&output);
    let stdout = text(&output.stdout);
    let version = stdout
        .strip_prefix("shdeps ")
        .and_then(|line| line.strip_suffix('\n'))
        .expect("version output should be a single shdeps line");
    let parts = version.split('-').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3, "unexpected version output: {stdout:?}");
    assert_eq!(parts[0].len(), 8, "unexpected version output: {stdout:?}");
    assert_eq!(parts[1].len(), 6, "unexpected version output: {stdout:?}");
    assert_eq!(parts[2].len(), 8, "unexpected version output: {stdout:?}");
    assert!(parts[0].bytes().all(|byte| byte.is_ascii_digit()));
    assert!(parts[1].bytes().all(|byte| byte.is_ascii_digit()));
    assert!(parts[2].bytes().all(|byte| byte.is_ascii_hexdigit()));
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
    assert!(
        stdout
            .contains("  dep-path <name> <rel>  Print a path below a configured dependency root\n")
    );
    assert!(stdout.contains(
        "  dep-file <name> <rel>  Print a readable regular file below a dependency root\n"
    ));
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
    fixture.write("conf/deps.conf", "cgraf78/sley  github\n");
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
fn mutating_api_installs_packages_with_cached_manager() {
    let fixture = Fixture::new("api-pkg-install");
    let fakebin = fixture.dir.join("fakebin");
    let log = fixture.dir.join("pkg.log");
    let path = format!("{}:/usr/bin:/bin", fakebin.display());
    fixture.write_executable(
        "fakebin/apt-cache",
        "#!/bin/sh\nprintf 'apt-cache %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$1:$2\" = show:tool ]\n",
    );
    fixture.write_executable(
        "fakebin/sudo",
        "#!/bin/sh\nprintf 'sudo %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\n",
    );

    let mut direct = fixture.command(["__api", "pkg-install", "tool"]);
    direct
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "apt")
        .env("SHDEPS_TEST_LOG", &log);
    let direct = run(&mut direct);

    assert_success(&direct);
    assert_eq!(text(&direct.stdout), "");
    assert_eq!(text(&direct.stderr), "");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "sudo apt-get update -qq\napt-cache show tool\nsudo apt-get install -y tool\n"
    );

    fs::write(&log, "").unwrap();
    let mut for_mgr = fixture.command(["__api", "pkg-install-for-mgr", "brew:other", "apt:tool"]);
    for_mgr
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "apt")
        .env("SHDEPS_TEST_LOG", &log);
    let for_mgr = run(&mut for_mgr);

    assert_success(&for_mgr);
    assert_eq!(text(&for_mgr.stdout), "");
    assert_eq!(text(&for_mgr.stderr), "");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "sudo apt-get update -qq\napt-cache show tool\nsudo apt-get install -y tool\n"
    );
}

#[test]
fn mutating_api_require_sudo_matches_quiet_prompt_rules() {
    let fixture = Fixture::new("api-require-sudo");
    let fakebin = fixture.dir.join("fakebin");
    let log = fixture.dir.join("sudo.log");
    let path = format!("{}:/usr/bin:/bin", fakebin.display());
    fixture.write_executable("fakebin/id", "#!/bin/sh\nprintf '1000\\n'\n");
    fixture.write_executable(
        "fakebin/sudo",
        "#!/bin/sh\nprintf 'sudo %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$1:$2\" = '-n:true' ] && exit 1\n[ \"$1\" = true ]\n",
    );

    let mut interactive = fixture.command(["__api", "require-sudo"]);
    interactive.env("PATH", &path).env("SHDEPS_TEST_LOG", &log);
    let interactive = run(&mut interactive);

    assert_success(&interactive);
    assert_eq!(text(&interactive.stdout), "");
    assert_eq!(text(&interactive.stderr), "");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "sudo -n true\nsudo true\n"
    );

    fs::write(&log, "").unwrap();
    let mut quiet = fixture.command(["__api", "require-sudo"]);
    quiet
        .env("PATH", &path)
        .env("SHDEPS_QUIET", "1")
        .env("SHDEPS_TEST_LOG", &log);
    let quiet = run(&mut quiet);

    assert_eq!(quiet.status.code(), Some(1));
    assert_eq!(text(&quiet.stdout), "");
    assert_eq!(text(&quiet.stderr), "");
    assert_eq!(fs::read_to_string(&log).unwrap(), "sudo -n true\n");
}

#[test]
fn mutating_api_installs_github_release_to_custom_path() {
    let fixture = Fixture::new("api-github-release");
    let fakebin = fixture.dir.join("fakebin");
    let archive = fixture.dir.join("release.tar.gz");
    let curl_log = fixture.dir.join("curl.log");
    let custom_bin = fixture.dir.join("launcher-owned/bin/mytool");
    let arch = host_arch();
    let asset = format!("mytool-v1.0.0-linux-{arch}.tar.gz");
    write_tar_gz(
        &archive,
        &[
            (
                "mytool-v1.0.0/bin/mytool",
                "#!/bin/sh\nprintf 'ok\\n'\n",
                0o755,
            ),
            (
                "mytool-v1.0.0/share/man/man1/mytool.1",
                ".TH MYTOOL 1\n",
                0o644,
            ),
        ],
    );
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/usr/bin/env bash
set -e
config=$(cat)
printf '%s\n---\n' "$config" >>"$SHDEPS_TEST_CURL_LOG"
case "$config" in
  *'url = "https://api.github.com/repos/owner/mytool/releases"'*)
    printf '[{"tag_name":"v1.0.0","assets":[{"name":"%s","browser_download_url":"https://downloads.example/%s"}]}]\n' \
      "$SHDEPS_TEST_ASSET" "$SHDEPS_TEST_ASSET"
    ;;
  *'url = "https://downloads.example/'"$SHDEPS_TEST_ASSET"'"'*)
    cat "$SHDEPS_TEST_ARCHIVE"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    let mut command = fixture.command([
        "__api",
        "github-release-install",
        "owner/mytool.git",
        "mytool",
        "owner/mytool.git",
        custom_bin.to_str().unwrap(),
    ]);
    command
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("GH_TOKEN", "bridge-token")
        .env("SHDEPS_TEST_ARCHIVE", &archive)
        .env("SHDEPS_TEST_ASSET", &asset)
        .env("SHDEPS_TEST_CURL_LOG", &curl_log)
        .env("SHDEPS_UPDATE_TXN_ID", "txn123");
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "  owner/mytool installed -- v1.0.0\n");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        fs::read_link(&custom_bin).unwrap(),
        fixture.dir.join("share/owner/mytool/bin/mytool")
    );
    assert!(!fixture.dir.join("bin/mytool").exists());
    assert_eq!(
        fs::read_link(fixture.dir.join("share/man/man1/mytool.1")).unwrap(),
        fixture
            .dir
            .join("share/owner/mytool/share/man/man1/mytool.1")
    );
    assert!(
        fixture
            .dir
            .join("state/.changed-markers/txn123/owner/mytool")
            .exists()
    );

    let log = fs::read_to_string(curl_log).unwrap();
    assert!(log.contains("url = \"https://api.github.com/repos/owner/mytool/releases\""));
    assert!(!log.contains("owner/mytool.git/releases"));
    assert_eq!(log.matches("Authorization: Bearer bridge-token").count(), 1);
}

#[test]
fn mutating_api_github_release_reports_selection_failures() {
    let fixture = Fixture::new("api-github-release-missing");
    let fakebin = fixture.dir.join("fakebin");
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/bin/sh
set -eu

# Keep this fake compatible with the production curl transport, which sends the
# request through a stdin config so auth headers do not leak through argv. Some
# curl versions treat a script that ignores stdin differently after the writer
# side closes; consuming the config makes this fixture behave consistently on
# the older CentOS Stream userspace in the shared CI matrix.
cat >/dev/null
printf '[{"tag_name":"v1.0.0","assets":[]}]\n'
"#,
    );

    let mut command = fixture.command([
        "__api",
        "github-release-install",
        "owner/missing",
        "missing",
    ]);
    command.env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()));
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "warning: owner/missing github release install failed: no matching release asset\n"
    );
    assert!(!fixture.dir.join("bin/missing").exists());
}

#[test]
fn rust_hook_prelude_delegates_link_extras_during_update() {
    let fixture = Fixture::new("hook-prelude-link-extras");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  mkdir -p "$SHDEPS_INSTALL_DIR/tool/share/man/man1"
  printf '.TH TOOL 1\n' >"$SHDEPS_INSTALL_DIR/tool/share/man/man1/tool.1"
  shdeps_link_extras tool "$SHDEPS_INSTALL_DIR/tool" || return $?
  printf 'installed\n'
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", shdeps_exe_dir().display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "==> Installing/upgrading tools...\n  tool: installed\n"
    );
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        fs::read_link(fixture.dir.join("share/man/man1/tool.1")).unwrap(),
        fixture.dir.join("share/tool/share/man/man1/tool.1")
    );
    assert!(fixture.dir.join("state/tool.links").exists());
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
fn cheap_path_and_status_commands_stay_within_ci_budget() {
    let fixture = Fixture::new("cheap-path-status-perf");
    let mut config = String::new();
    for index in 0..100 {
        config.push_str(&format!("owner/tool-{index:03}  github:repo\n"));
    }
    config.push_str("cgraf78/sley  github:repo\n");
    config.push_str("asset github:release asset\n");
    fixture.write("conf/deps.conf", &config);
    fixture.write("share/cgraf78/sley/share/sley/shell.sh", "SLEY=installed\n");
    fixture.write_executable("bin/asset", "#!/bin/sh\n");
    fixture.write(
        "state/manifest",
        &format!(
            "asset|github:release|asset|{}\n",
            fixture.dir.join("bin/asset").display()
        ),
    );

    // Warm the binary once, then time real subprocess invocations. These
    // commands sit on editor and shell-integration paths, so the guard is about
    // catching obvious startup/config-load regressions rather than claiming a
    // precise benchmark number.
    assert_success(&run(&mut fixture.command(["version"])));

    let (dep_root, root_elapsed) = timed(&mut fixture.command(["dep-root", "cgraf78/sley"]));
    assert_success(&dep_root);
    assert!(
        root_elapsed <= Duration::from_millis(200),
        "dep-root should stay under the CI cheap-command budget; elapsed={root_elapsed:?}, stdout={:?}, stderr={:?}",
        text(&dep_root.stdout),
        text(&dep_root.stderr)
    );

    let (dep_path, path_elapsed) =
        timed(&mut fixture.command(["dep-path", "cgraf78/sley", "share/sley/shell.sh"]));
    assert_success(&dep_path);
    assert!(
        path_elapsed <= Duration::from_millis(200),
        "dep-path should stay under the CI cheap-command budget; elapsed={path_elapsed:?}, stdout={:?}, stderr={:?}",
        text(&dep_path.stdout),
        text(&dep_path.stderr)
    );

    let (check, check_elapsed) = timed(&mut fixture.command(["check", "asset"]));
    assert_success(&check);
    assert_eq!(text(&check.stdout), "asset: installed\n");
    assert!(
        check_elapsed <= Duration::from_millis(300),
        "manifest-backed check should stay under the CI budget; elapsed={check_elapsed:?}, stdout={:?}, stderr={:?}",
        text(&check.stdout),
        text(&check.stderr)
    );
}

#[test]
fn no_op_manifest_backed_update_stays_fast_and_skips_network_and_tools() {
    let fixture = Fixture::new("noop-update-perf");
    fixture.write(
        "conf/deps.conf",
        "owner/tool github:release tool\nripgrep cargo rg\ngithub.com/junegunn/fzf go fzf\nruff uv\nprettier npm\n",
    );
    fixture.write_executable("bin/tool", "#!/bin/sh\n");
    fixture.write_executable("share/ripgrep/bin/rg", "#!/bin/sh\n");
    fixture.write_executable("share/github.com/junegunn/fzf/bin/fzf", "#!/bin/sh\n");
    fixture.write_executable("share/ruff/bin/ruff", "#!/bin/sh\n");
    fixture.write_executable("share/prettier/bin/prettier", "#!/bin/sh\n");
    for (name, kind) in [
        ("owner/tool", "release"),
        ("ripgrep", "cargo"),
        ("github.com/junegunn/fzf", "go"),
        ("ruff", "uv"),
        ("prettier", "npm"),
    ] {
        fixture.write_fresh_stamp(name, kind);
    }

    let fakebin = fixture.dir.join("fakebin");
    for command in ["curl", "cargo", "go", "uv", "npm"] {
        fixture.write_executable(
            fakebin.join(command).strip_prefix(&fixture.dir).unwrap(),
            "#!/bin/sh\nprintf 'unexpected warm-path command: %s\\n' \"$0\" >&2\nexit 99\n",
        );
    }
    let path = format!("{}:/usr/bin:/bin", fakebin.display());

    // Warm once with the same network/tool-denying PATH. If a future change
    // accidentally makes a fresh manifest-backed update touch GitHub or a
    // language installer, the fake command fails deterministically instead of
    // only showing up as a slow benchmark.
    let mut warm = fixture.command(["update"]);
    warm.env("PATH", &path);
    assert_success(&run(&mut warm));

    let mut command = fixture.command(["update"]);
    command.env("PATH", &path);
    let (output, elapsed) = timed(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "==> Installing/upgrading tools...\n");
    assert_eq!(text(&output.stderr), "");
    assert!(
        elapsed <= Duration::from_secs(1),
        "warm manifest-backed update should stay under the CI budget; elapsed={elapsed:?}, stdout={:?}, stderr={:?}",
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
fn list_resolves_bare_github_to_concrete_release_method() {
    let fixture = Fixture::new("list-github-release");
    let asset = host_linux_asset("tool", "v1.0.0");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("bin/tool", "#!/bin/sh\nprintf '1.0.0\\n'\n");
    fixture.write(
        "state/manifest",
        &format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.dir.join("bin/tool").display()
        ),
    );
    fixture.write_fake_curl(&release_json("v1.0.0", &[asset.as_str()]), "unused");

    let mut command = fixture.command(["list"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert!(text(&output.stdout).contains("owner/tool github:release installed"));
    assert!(!text(&output.stdout).contains(" github "));
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
fn update_bare_github_prefers_release_and_records_concrete_manifest_method() {
    let fixture = Fixture::new("update-github-release");
    let asset = host_linux_asset("tool", "v1.0.0");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\nprintf 'local clone\\n'\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[asset.as_str()]),
        "#!/bin/sh\nprintf 'release asset\\n'\n",
    );

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_TEST_CURL_LOG", fixture.dir.join("fake/curl.log"));
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("bin/tool")).unwrap(),
        "#!/bin/sh\nprintf 'release asset\\n'\n"
    );
    assert!(!fixture.dir.join("share/owner/tool").exists());
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/manifest")).unwrap(),
        format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.dir.join("bin/tool").display()
        )
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("fake/curl.log")).unwrap(),
        "api\nasset\n",
        "bare github resolution should reuse release metadata during install"
    );
}

#[test]
fn update_explicit_github_release_fetches_without_bare_github_cache() {
    let fixture = Fixture::new("update-explicit-github-release");
    let asset = host_linux_asset("tool", "v1.0.0");
    fixture.write("conf/deps.conf", "owner/tool github:release tool\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[asset.as_str()]),
        "#!/bin/sh\nprintf 'release asset\\n'\n",
    );

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_TEST_CURL_LOG", fixture.dir.join("fake/curl.log"));
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("bin/tool")).unwrap(),
        "#!/bin/sh\nprintf 'release asset\\n'\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("fake/curl.log")).unwrap(),
        "api\nasset\n",
        "explicit github:release should fetch normally when no resolver cache exists"
    );
}

#[test]
fn update_explicit_github_repo_does_not_fetch_release_metadata() {
    let fixture = Fixture::new("update-explicit-github-repo");
    fixture.write("conf/deps.conf", "owner/tool github:repo tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\nprintf 'local clone\\n'\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[host_linux_asset("tool", "v1.0.0").as_str()]),
        "unused",
    );

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_TEST_CURL_LOG", fixture.dir.join("fake/curl.log"));
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("bin/tool")).unwrap(),
        "#!/bin/sh\nprintf 'local clone\\n'\n"
    );
    assert!(
        !fixture.dir.join("fake/curl.log").exists(),
        "explicit github:repo should not consult release metadata"
    );
}

#[test]
fn update_bare_github_falls_back_to_repo_and_uses_local_clone() {
    let fixture = Fixture::new("update-github-repo");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\nprintf 'local clone\\n'\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &["tool-v1.0.0-darwin-aarch64"]),
        "unused",
    );

    let mut command = fixture.command(["update"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_link(fixture.dir.join("share/owner/tool")).unwrap(),
        fixture.dir.join("git/tool")
    );
    assert_eq!(
        fs::read_link(fixture.dir.join("bin/tool")).unwrap(),
        fixture.dir.join("share/owner/tool/bin/tool")
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("owner/tool|github:repo|tool|")
    );
}

#[test]
fn update_bare_github_transitions_repo_to_release_after_release_appears() {
    let fixture = Fixture::new("update-github-repo-to-release");
    let asset = host_linux_asset("tool", "v1.0.0");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("share/owner/tool/bin/tool", "#!/bin/sh\nprintf 'repo\\n'\n");
    fixture.write(
        "state/manifest",
        &format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.dir.join("share/owner/tool").display()
        ),
    );
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[asset.as_str()]),
        "#!/bin/sh\nprintf 'release asset\\n'\n",
    );

    let mut command = fixture.command(["--force", "update"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert!(!fixture.dir.join("share/owner/tool").exists());
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("owner/tool|github:release|tool|")
    );
}

#[test]
fn update_bare_github_rechecks_legacy_repo_cache_and_transitions_to_release() {
    let fixture = Fixture::new("update-github-legacy-repo-cache");
    let asset = host_linux_asset("tool", "v1.0.0");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("share/owner/tool/bin/tool", "#!/bin/sh\nprintf 'repo\\n'\n");
    fixture.write(
        "state/manifest",
        &format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.dir.join("share/owner/tool").display()
        ),
    );
    fixture.write("state/owner/tool.github.method", "github:repo\n");
    fixture.write_fresh_stamp("owner/tool", "github");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[asset.as_str()]),
        "#!/bin/sh\nprintf 'release asset\\n'\n",
    );

    let mut command = fixture.command(["--force", "update"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("bin/tool")).unwrap(),
        "#!/bin/sh\nprintf 'release asset\\n'\n"
    );
    assert!(!fixture.dir.join("share/owner/tool").exists());
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/owner/tool.github.method")).unwrap(),
        "github:release\n"
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("owner/tool|github:release|tool|")
    );
}

#[test]
fn update_bare_github_transitions_release_to_repo_when_release_is_unavailable() {
    let fixture = Fixture::new("update-github-release-to-repo");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\nprintf 'local clone\\n'\n");
    fixture.write_executable("bin/tool", "#!/bin/sh\nprintf 'old release\\n'\n");
    fixture.write(
        "state/manifest",
        &format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.dir.join("bin/tool").display()
        ),
    );
    fixture.write_fake_curl(
        &release_json("v1.0.0", &["tool-v1.0.0-darwin-aarch64"]),
        "unused",
    );

    let mut command = fixture.command(["update"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        fs::read_link(fixture.dir.join("share/owner/tool")).unwrap(),
        fixture.dir.join("git/tool")
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("owner/tool|github:repo|tool|")
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
fn update_quiet_environment_suppresses_normal_output() {
    let fixture = Fixture::new("update-quiet-env");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
  printf 'installed\n'
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_QUIET", "1").env(
        "PATH",
        format!("{}:/usr/bin:/bin", shdeps_exe_dir().display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/tool-installed")).unwrap(),
        "installed\n"
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("tool|custom|tool|")
    );
}

#[test]
fn update_quiet_environment_suppresses_empty_config_message() {
    let fixture = Fixture::new("update-empty-quiet-env");

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_QUIET", "1");
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "");
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
    assert_eq!(
        text(&first.stdout),
        "==> Installing/upgrading tools...\n  tool: 1.2.3\n"
    );
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
    assert_eq!(text(&second.stdout), "==> Installing/upgrading tools...\n");
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
    assert_eq!(text(&output.stdout), "==> Installing/upgrading tools...\n");
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
    assert_eq!(
        text(&output.stdout),
        "==> Installing/upgrading tools...\n  tool: installed\n"
    );
    assert_eq!(
        text(&output.stderr),
        "==> 1 orphaned dep(s) no longer in config:\n  old (github:release)\nRun `shdeps prune` to remove orphaned artifacts.\n"
    );
    assert!(fixture.dir.join("bin/old").exists());
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("old|github:release|old|")
    );
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
    assert!(
        !fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("old|")
    );
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
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("pkg-tool|")
    );

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
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .is_empty()
    );
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

#[test]
fn self_update_release_archive_install_updates_through_cli() {
    let fixture = Fixture::new("self-update-release-cli");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let archive = fixture.dir.join("shdeps-release.tar.gz");
    let checksum = fixture.dir.join("shdeps-release.tar.gz.sha256");
    let arch = host_arch();
    let platform = format!("linux-{arch}-musl");
    fs::create_dir_all(&install).unwrap();
    fixture.write("release-install/shdeps", "old binary\n");
    fixture.write("release-install/shdeps.sh", "old shim\n");
    // Release self-update is intentionally driven from the installed metadata,
    // not from the current working tree. That is the important migration case
    // for fleet machines after install.sh has replaced the Bash checkout with a
    // standalone release bundle.
    fixture.write(
        "release-install/.shdeps-install.json",
        &format!(
            r#"{{"schema":1,"method":"release","artifact_platform":"{platform}","tag":"20260523-120000-cafebabe","repo":"cgraf78/shdeps"}}"#
        ),
    );

    let archive_name = format!("shdeps-20260524-120000-deadbeef-{platform}.tar.gz");
    let checksum_name = format!("{archive_name}.sha256");
    write_tar_gz(
        &archive,
        &[
            (
                "shdeps",
                "#!/bin/sh\nprintf 'shdeps 20260524-120000-deadbeef\\n'\n",
                0o755,
            ),
            ("shdeps.sh", "shdeps_version() { :; }\n", 0o644),
            ("install.sh", "#!/bin/sh\nexit 0\n", 0o755),
            ("README.md", "readme\n", 0o644),
            ("LICENSE", "license\n", 0o644),
            ("man/man1/shdeps.1", ".TH SHDEPS 1\n", 0o644),
        ],
    );
    fs::write(
        &checksum,
        format!(
            "{}  {archive_name}\n",
            shdeps::checksum::sha256_hex(&fs::read(&archive).unwrap())
        ),
    )
    .unwrap();

    fixture.write_executable(
        "fakebin/curl",
        r#"#!/usr/bin/env bash
set -e
config=$(cat)
# Production sends URLs through curl's stdin config so tokens never appear in
# process argv. The fake keeps that contract visible instead of accepting argv
# shortcuts that the real transport does not use.
case "$config" in
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases"'*)
    printf '[{"tag_name":"20260524-120000-deadbeef","draft":false,"prerelease":false,"assets":[{"name":"%s","browser_download_url":"https://downloads.example/%s"},{"name":"%s","browser_download_url":"https://downloads.example/%s"}]}]\n' \
      "$SHDEPS_TEST_ARCHIVE_NAME" "$SHDEPS_TEST_ARCHIVE_NAME" \
      "$SHDEPS_TEST_CHECKSUM_NAME" "$SHDEPS_TEST_CHECKSUM_NAME"
    ;;
  *'url = "https://downloads.example/'"$SHDEPS_TEST_ARCHIVE_NAME"'"'*)
    cat "$SHDEPS_TEST_ARCHIVE"
    ;;
  *'url = "https://downloads.example/'"$SHDEPS_TEST_CHECKSUM_NAME"'"'*)
    cat "$SHDEPS_TEST_CHECKSUM"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    let mut command = fixture.command(["self-update"]);
    command
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_ARCHIVE", &archive)
        .env("SHDEPS_TEST_CHECKSUM", &checksum)
        .env("SHDEPS_TEST_ARCHIVE_NAME", &archive_name)
        .env("SHDEPS_TEST_CHECKSUM_NAME", &checksum_name);
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "shdeps: updated to 20260524-120000-deadbeef\n"
    );
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        fs::read_to_string(install.join("shdeps")).unwrap(),
        "#!/bin/sh\nprintf 'shdeps 20260524-120000-deadbeef\\n'\n"
    );
    let metadata = shdeps::install_metadata::read(&install).unwrap();
    assert!(
        matches!(metadata, shdeps::install_metadata::Read::Valid(metadata)
            if metadata.tag.as_deref() == Some("20260524-120000-deadbeef")
                && metadata.artifact_platform.as_deref() == Some(platform.as_str())
                && metadata.repo.as_deref() == Some("cgraf78/shdeps"))
    );

    let mut quiet = fixture.command(["self-update"]);
    quiet
        .env("SHDEPS_DIR", &install)
        .env("SHDEPS_QUIET", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_ARCHIVE", &archive)
        .env("SHDEPS_TEST_CHECKSUM", &checksum)
        .env("SHDEPS_TEST_ARCHIVE_NAME", &archive_name)
        .env("SHDEPS_TEST_CHECKSUM_NAME", &checksum_name);
    let quiet = run(&mut quiet);

    assert_success(&quiet);
    assert_eq!(text(&quiet.stdout), "");
    assert_eq!(text(&quiet.stderr), "");
}

fn run(command: &mut Command) -> Output {
    command.output().expect("shdeps command should run")
}

fn timed(command: &mut Command) -> (Output, Duration) {
    let started = Instant::now();
    let output = run(command);
    (output, started.elapsed())
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

fn host_arch() -> String {
    let output = Command::new("uname")
        .arg("-m")
        .output()
        .expect("uname should be available in CLI tests");
    match text(&output.stdout).trim() {
        // Release labels normalize common architecture aliases. Keep the test
        // fixtures on the same canonical spelling so macOS arm64 runners do not
        // accidentally publish `linux-arm64-musl`, which is not a shdeps asset
        // contract.
        "arm64" => "aarch64".to_owned(),
        "amd64" => "x86_64".to_owned(),
        arch => arch.to_owned(),
    }
}

fn host_linux_asset(cmd: &str, tag: &str) -> String {
    // The CLI fixtures force shdeps' logical platform to Linux so release
    // selection exercises one stable asset naming path on every CI runner.
    // Asset matching still asks the host for `uname -m`, though, so keep the
    // fixture architecture aligned with the actual runner instead of assuming
    // x86_64. That catches real matching behavior without making ARM macOS CI
    // look like a missing-release fallback.
    format!("{cmd}-{tag}-linux-{}", host_arch())
}

fn shdeps_exe_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_shdeps"))
        .parent()
        .expect("shdeps test binary should have a parent directory")
        .to_path_buf()
}

fn write_tar_gz(path: &Path, entries: &[(&str, &str, u32)]) {
    let file = fs::File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (name, body, mode) in entries {
        let bytes = body.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
}

fn release_json(tag: &str, assets: &[&str]) -> String {
    let assets = assets
        .iter()
        .map(|asset| {
            format!(
                r#"{{"name":"{asset}","browser_download_url":"https://downloads.example/{asset}"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"[{{"tag_name":"{tag}","draft":false,"prerelease":false,"assets":[{assets}]}}]"#)
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
            .env(
                "SHDEPS_TEST_RELEASE_JSON",
                self.dir.join("fake/release.json"),
            )
            .env("SHDEPS_TEST_RELEASE_ASSET", self.dir.join("fake/asset"))
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

    fn write_fake_curl(&self, releases_json: &str, asset_body: &str) {
        self.write("fake/release.json", releases_json);
        self.write("fake/asset", asset_body);
        self.write_executable(
            "fakebin/curl",
            r#"#!/usr/bin/env bash
set -euo pipefail
config=$(cat)
case "$config" in
  *'url = "https://api.github.com/repos/owner/tool/releases"'*)
    if [[ -n "${SHDEPS_TEST_CURL_LOG:-}" ]]; then
      printf 'api\n' >>"$SHDEPS_TEST_CURL_LOG"
    fi
    cat "$SHDEPS_TEST_RELEASE_JSON"
    ;;
  *'url = "https://downloads.example/'*)
    if [[ -n "${SHDEPS_TEST_CURL_LOG:-}" ]]; then
      printf 'asset\n' >>"$SHDEPS_TEST_CURL_LOG"
    fi
    cat "$SHDEPS_TEST_RELEASE_ASSET"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
        );
    }

    fn write_fresh_stamp(&self, name: &str, kind: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_secs();
        let path = shdeps::stamp::remote_path(&self.dir.join("state"), name, kind);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{now}\n")).unwrap();
    }
}
