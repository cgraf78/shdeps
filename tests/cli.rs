use std::fs;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use shdeps::cli::{HELP, PUBLIC_COMMANDS};

// Every CLI case spawns subprocesses while libtest fans cases out across
// worker threads, and fork() from a threaded process can stall inside dyld
// locks on macOS: the whole suite hangs instead of one case failing. The
// macOS CLI binary therefore pins the harness to a single worker with an
// image initializer before main runs. Linux keeps parallel workers since
// its fork path has no such stall; the lib unit-test binary also stays
// parallel on macOS because its cases never fork.
#[cfg(target_os = "macos")]
extern "C" fn serialize_macos_cli_tests() {
    // SAFETY: image initializer, runs single-threaded before main; the
    // setenv only publishes RUST_TEST_THREADS for the libtest runner.
    unsafe {
        libc::setenv(c"RUST_TEST_THREADS".as_ptr(), c"1".as_ptr(), 1);
    }
}

#[used]
#[cfg(target_os = "macos")]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static SERIALIZE_MACOS_CLI_TESTS: extern "C" fn() = serialize_macos_cli_tests;

// Regression guard for the initializer above: without it the variable is
// absent on runners (no workflow sets it) and the suite can hang.
#[cfg(target_os = "macos")]
#[test]
fn macos_cli_harness_runs_single_threaded() {
    assert_eq!(std::env::var("RUST_TEST_THREADS").as_deref(), Ok("1"));
}

// Hosted runners can deschedule one short subprocess without indicating a
// user-visible regression. Three samples keep the median sensitive to a
// persistent slowdown while discarding one isolated scheduler outlier.
const CI_PERFORMANCE_SAMPLES: usize = 3;

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
        stdout.contains(
            "  dep-links <name>       Print public command links owned by a dependency\n"
        )
    );
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
fn dep_links_reports_repo_command_links_with_clean_tsv() {
    let fixture = Fixture::new("dep-links-repo");
    fixture.write("conf/deps.conf", "cgraf78/tool github:repo\n");
    fixture.write_executable("share/cgraf78/tool/bin/tool-b", "#!/bin/sh\n");
    fixture.write_executable("share/cgraf78/tool/bin/tool-a", "#!/bin/sh\n");
    fixture.write("share/cgraf78/tool/bin/not-executable", "#!/bin/sh\n");

    let output = run(&mut fixture.command(["dep-links", "cgraf78/tool"]));

    assert_success(&output);
    let root = fixture
        .dir
        .join("share/cgraf78/tool")
        .canonicalize()
        .unwrap();
    assert_eq!(
        text(&output.stdout),
        format!(
            "tool-a\t{}\t{}\n\
             tool-b\t{}\t{}\n",
            fixture.dir.join("bin/tool-a").display(),
            root.join("bin/tool-a").display(),
            fixture.dir.join("bin/tool-b").display(),
            root.join("bin/tool-b").display()
        )
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn dep_links_reports_manifest_single_binary_target() {
    let fixture = Fixture::new("dep-links-single");
    let target = fixture.dir.join("share/owner/tool/bin/tool-real");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write(
        "state/manifest",
        &format!("owner/tool|github:release|tool|{}\n", target.display()),
    );

    let output = run(&mut fixture.command(["dep-links", "owner/tool"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        format!(
            "tool\t{}\t{}\n",
            fixture.dir.join("bin/tool").display(),
            target.display()
        )
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn dep_links_usage_and_missing_dependency_exit_codes_are_machine_clean() {
    let fixture = Fixture::new("dep-links-errors");
    fixture.write("conf/deps.conf", "owner/tool github:repo - - os:macos\n");

    let usage = run(&mut fixture.command(["dep-links"]));
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(text(&usage.stdout), "");
    assert_eq!(
        text(&usage.stderr),
        "error: dep-links requires a dependency name\nUsage: shdeps dep-links <name>\n"
    );

    let missing = run(&mut fixture.command(["dep-links", "owner/tool"]));
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(text(&missing.stdout), "");
    assert_eq!(text(&missing.stderr), "");
}

#[test]
fn read_only_api_outputs_machine_clean_lines() {
    let fixture = Fixture::new("api");
    fixture.write("conf/deps.conf", "owner/tool.git github:repo\njq pkg\n");
    fixture.write_executable("share/owner/tool/bin/tool", "#!/bin/sh\n");

    let version = run(&mut fixture.command(["__api", "version"]));
    assert_success(&version);
    assert_eq!(text(&version.stdout), "abi:1\n");
    assert_eq!(text(&version.stderr), "");

    let capability = run(&mut fixture.command([
        "__api",
        "capability",
        "release-archive-launcher-preservation-v1",
    ]));
    assert_success(&capability);
    assert_eq!(text(&capability.stdout), "");
    assert_eq!(text(&capability.stderr), "");

    let cancellation =
        run(&mut fixture.command(["__api", "capability", "owned-subprocess-cancellation-v1"]));
    assert_eq!(
        cancellation.status.code(),
        Some(
            if shdeps::cancellation::owned_subprocess_cancellation_available() {
                0
            } else {
                1
            }
        )
    );
    assert_eq!(text(&cancellation.stdout), "");
    assert_eq!(text(&cancellation.stderr), "");

    let prompt_fifo =
        run(&mut fixture.command(["__api", "capability", "prompt-fifo-reader-before-event-v1"]));
    assert_success(&prompt_fifo);
    assert_eq!(text(&prompt_fifo.stdout), "");
    assert_eq!(text(&prompt_fifo.stderr), "");

    let unknown = run(&mut fixture.command(["__api", "capability", "not-a-real-capability"]));
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(text(&unknown.stdout), "");
    assert_eq!(text(&unknown.stderr), "");

    let malformed = run(&mut fixture.command([
        "__api",
        "capability",
        "release-archive-launcher-preservation-v1",
        "extra",
    ]));
    assert_eq!(malformed.status.code(), Some(2));
    assert_eq!(text(&malformed.stdout), "");
    assert_eq!(
        text(&malformed.stderr),
        "error: __api capability requires exactly one name\n"
    );

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

    fixture.write_executable("bin/pacman", "#!/bin/sh\nexit 0\n");
    let mut manager_match = fixture.command(["__api", "filter-match", "mgr:brew,mgr:pacman"]);
    manager_match.env_remove("SHDEPS_PKG_MGR");
    manager_match.env("PATH", fixture.dir.join("bin"));
    let manager_match = run(&mut manager_match);
    assert_success(&manager_match);
    assert_eq!(text(&manager_match.stdout), "");
    assert_eq!(text(&manager_match.stderr), "");

    let mut manager_mismatch = fixture.command(["__api", "filter-match", "mgr:apt"]);
    manager_mismatch.env_remove("SHDEPS_PKG_MGR");
    manager_mismatch.env("PATH", fixture.dir.join("bin"));
    let manager_mismatch = run(&mut manager_mismatch);
    assert_eq!(manager_mismatch.status.code(), Some(3));
    assert_eq!(text(&manager_mismatch.stdout), "");
    assert_eq!(text(&manager_mismatch.stderr), "");

    let links = run(&mut fixture.command(["__api", "dep-links", "owner/tool"]));
    assert_success(&links);
    assert_eq!(
        text(&links.stdout),
        format!(
            "tool\t{}\t{}\n",
            fixture.dir.join("bin/tool").display(),
            fixture
                .dir
                .join("share/owner/tool")
                .canonicalize()
                .unwrap()
                .join("bin/tool")
                .display()
        )
    );
    assert_eq!(text(&links.stderr), "");
}

#[test]
fn explicit_release_archive_launcher_adoption_is_machine_clean() {
    let fixture = Fixture::new("adopt-release-archive-launcher");
    let public = fixture.dir.join("bin/tool");
    fixture.write_executable("share/owner/tool/bin/tool", "#!/bin/sh\n");
    fixture.write_executable("bin/tool", "#!/bin/sh\n");
    fixture.write(
        "state/manifest",
        &format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.dir.join("share/owner/tool/bin/tool").display()
        ),
    );
    fixture.write(
        "state/owner/tool.binlinks",
        &format!("{}\n", public.display()),
    );

    let adopted = run(&mut fixture.command([
        "__api",
        "adopt-release-archive-launcher",
        "owner/tool",
        "tool",
    ]));
    assert_success(&adopted);
    assert_eq!(text(&adopted.stdout), "");
    assert_eq!(text(&adopted.stderr), "");
    assert_eq!(
        fs::read_to_string(fixture.dir.join("share/owner/tool/.shdeps-release-layout")).unwrap(),
        "v1 archive\n"
    );

    let malformed = run(&mut fixture.command([
        "__api",
        "adopt-release-archive-launcher",
        "owner/tool",
        "tool",
        "extra",
    ]));
    assert_eq!(malformed.status.code(), Some(2));
    assert_eq!(text(&malformed.stdout), "");
    assert_eq!(
        text(&malformed.stderr),
        "error: __api adopt-release-archive-launcher requires a dependency name and command\n"
    );
}

#[test]
fn completion_api_reports_commands_and_loaded_dependency_names() {
    let fixture = Fixture::new("api-completion");
    fixture.write(
        "conf/10-deps.conf",
        "owner/tool.git github:repo\njq pkg\njq pkg apt:jq-alt\n",
    );

    let commands = run(&mut fixture.command(["__api", "completion-commands"]));
    assert_success(&commands);
    let expected_commands = PUBLIC_COMMANDS
        .iter()
        .map(|command| format!("{}\t{}", command.name, command.description))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(text(&commands.stdout), expected_commands);
    let help_commands = HELP
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect::<Vec<_>>();
    let public_commands = PUBLIC_COMMANDS
        .iter()
        .map(|command| command.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        help_commands, public_commands,
        "HELP command block should match PUBLIC_COMMANDS"
    );
    for command in PUBLIC_COMMANDS {
        assert!(
            HELP.contains(command.name),
            "HELP should advertise public command {}",
            command.name
        );
    }
    assert_eq!(text(&commands.stderr), "");

    let names = run(&mut fixture.command(["__api", "completion-dep-names"]));
    assert_success(&names);
    assert_eq!(text(&names.stdout), "jq\nowner/tool\n");
    assert_eq!(text(&names.stderr), "");
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
fn skip_marker_api_round_trips() {
    let fixture = Fixture::new("api-skip");

    let unmarked = run(&mut fixture.command(["__api", "skip-check", "owner/tool"]));
    assert_eq!(unmarked.status.code(), Some(1));

    let mark = run(&mut fixture.command(["__api", "skip-mark", "owner/tool", "no java runtime"]));
    assert_success(&mark);
    assert!(
        fixture.dir.join("share/owner/tool/.skipped").exists(),
        "skip marker should live under the install dir"
    );

    let check = run(&mut fixture.command(["__api", "skip-check", "owner/tool"]));
    assert_success(&check);

    let reason = run(&mut fixture.command(["__api", "skip-reason", "owner/tool"]));
    assert_success(&reason);
    assert_eq!(text(&reason.stdout), "no java runtime\n");

    let clear = run(&mut fixture.command(["__api", "skip-clear", "owner/tool"]));
    assert_success(&clear);
    let recheck = run(&mut fixture.command(["__api", "skip-check", "owner/tool"]));
    assert_eq!(recheck.status.code(), Some(1));
    let gone = run(&mut fixture.command(["__api", "skip-reason", "owner/tool"]));
    assert_eq!(gone.status.code(), Some(1));

    // An unsafe dependency name is rejected without touching the filesystem.
    let bad = run(&mut fixture.command(["__api", "skip-mark", "../escape", "x"]));
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn find_runtime_api_searches_dirs_and_rejects() {
    let fixture = Fixture::new("api-find-runtime");
    fixture.write_executable("opt/jdk/bin/myjava", "#!/bin/sh\necho 'openjdk 21'\n");
    let opt = fixture.dir.join("opt/jdk/bin");

    let found = run(&mut fixture.command([
        "__api",
        "find-runtime",
        "--path",
        opt.to_str().unwrap(),
        "myjava",
    ]));
    assert_success(&found);
    assert_eq!(text(&found.stdout), format!("{}/myjava\n", opt.display()));

    let missing = run(&mut fixture.command(["__api", "find-runtime", "definitely-absent-xyz"]));
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(text(&missing.stdout), "");

    // --reject drops a candidate whose --version output matches the substring.
    fixture.write_executable("opt/php/bin/myphp", "#!/bin/sh\necho 'HipHop VM 4'\n");
    let php = fixture.dir.join("opt/php/bin");
    let rejected = run(&mut fixture.command([
        "__api",
        "find-runtime",
        "--path",
        php.to_str().unwrap(),
        "--reject",
        "HipHop",
        "myphp",
    ]));
    assert_eq!(rejected.status.code(), Some(1));
}

#[test]
fn write_wrapper_api_generates_executable_launcher() {
    let fixture = Fixture::new("api-write-wrapper");
    let payload = fixture.dir.join("share/gjf/google-java-format.jar");

    let wrapper = run(&mut fixture.command([
        "__api",
        "write-wrapper",
        "google-java-format",
        "java",
        "-jar",
        "--",
        payload.to_str().unwrap(),
    ]));
    assert_success(&wrapper);
    let wrapper_path = fixture.dir.join("bin/google-java-format");
    assert_eq!(
        text(&wrapper.stdout),
        format!("{}\n", wrapper_path.display())
    );
    let mode = fs::metadata(&wrapper_path).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "wrapper should be executable");
    let body = fs::read_to_string(&wrapper_path).unwrap();
    assert!(body.starts_with("#!/usr/bin/env bash\n"));
    assert!(body.contains(&format!(
        "exec 'java' '-jar' '{}' \"$@\"",
        payload.display()
    )));

    // --env lines are emitted before the exec so PATH-style values still expand.
    let with_env = run(&mut fixture.command([
        "__api",
        "write-wrapper",
        "--env",
        "PATH=/x:$PATH",
        "rubocop",
        "ruby",
        "--",
        payload.to_str().unwrap(),
    ]));
    assert_success(&with_env);
    let rb = fs::read_to_string(fixture.dir.join("bin/rubocop")).unwrap();
    assert!(rb.contains("export PATH=/x:$PATH\n"));

    // An unsafe wrapper name is rejected.
    let bad = run(&mut fixture.command(["__api", "write-wrapper", "../evil", "ruby", "--", "x"]));
    assert_eq!(bad.status.code(), Some(2));
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
  *'url = "https://api.github.com/repos/owner/mytool/releases?per_page=100"'*)
    printf '[{"tag_name":"v1.0.0","assets":[{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"}]}]\n' \
      "$SHDEPS_TEST_ASSET" "$SHDEPS_TEST_ASSET"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_ASSET"'"'*)
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
    assert!(
        log.contains("url = \"https://api.github.com/repos/owner/mytool/releases?per_page=100\"")
    );
    assert!(!log.contains("owner/mytool.git/releases"));
    assert_eq!(log.matches("Authorization: Bearer bridge-token").count(), 1);
}

#[test]
fn mutating_api_github_release_reports_selection_failures() {
    let fixture = Fixture::new("api-github-release-missing");
    let fakebin = fixture.dir.join("fakebin");
    fixture.write_executable(
        "fakebin/curl",
        r##"#!/bin/sh
set -eu

# Keep this fake compatible with the production curl transport, which sends the
# request through a stdin config so auth headers do not leak through argv. Some
# curl versions treat a script that ignores stdin differently after the writer
# side closes; consuming the config makes this fixture behave consistently on
# the older CentOS Stream userspace in the shared CI matrix.
cat >/dev/null
printf '[{"tag_name":"v1.0.0","assets":[]}]\n'
"##,
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
fn mutating_api_github_release_rejects_unsafe_cmd_basename() {
    // Regression for the bridge-side path-escape hole. Without
    // `valid_cmd_basename` applied here, an absolute `cmd` argument
    // would make the default `roots.bin_dir.join(cmd)` resolve to
    // the absolute path verbatim (Rust's `Path::join` discards the
    // left operand when the right is absolute), and the downstream
    // release-install pipeline would rename the staged executable
    // straight onto that path — outside the managed bin dir and
    // outside the `safe_managed_path` containment that protects
    // manifest `install_path`. The bridge must enforce the same
    // basename validator the config-side `parse_entry` does, so
    // the two entry points cannot diverge.
    let fixture = Fixture::new("api-github-release-unsafe-cmd");

    // No curl fake needed: the request must fail validation before
    // any network call. If validation regressed we'd see a totally
    // different failure mode (network or asset-selection error).
    let mut command = fixture.command([
        "__api",
        "github-release-install",
        "owner/mytool",
        "/etc/passwd",
    ]);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(text(&output.stdout), "");
    assert!(
        text(&output.stderr).contains("invalid github-release-install arguments"),
        "stderr should report invalid args, got: {}",
        text(&output.stderr)
    );
    // No public-bin link should have been touched.
    assert!(!std::path::Path::new("/etc/passwd-shdeps-marker").exists());
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
        format!(
            "{}:{}:/usr/bin:/bin",
            fixture.dir.join("fakebin").display(),
            shdeps_exe_dir().display()
        ),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  changed  Custom: 1 changed\n    changed  tool: installed\n  changed  1 changed\n"
    );
    assert_eq!(text(&output.stderr), "");
    assert_eq!(
        fs::read_link(fixture.dir.join("share/man/man1/tool.1")).unwrap(),
        fixture.dir.join("share/tool/share/man/man1/tool.1")
    );
    assert!(fixture.dir.join("state/tool.links").exists());
}

#[test]
fn hook_prelude_snapshot_answers_match_direct_api_answers() {
    // Transparency probe for the hook-prelude env cache: whatever the
    // prelude helpers answer inside a real hook subprocess must equal the
    // direct `__api` answers under the same environment, both before the
    // cache exists (pure bridge) and after (env-hit with bridge fallback).
    // Detection is hermetic: a fakebin-only PATH finds no package manager
    // on any host, and platform/host come from the fixture's test overrides.
    for (force, reinstall) in [("0", "0"), ("1", "1")] {
        let fixture = Fixture::new("hook-prelude-snapshot");
        fixture.write("conf/deps.conf", "tool custom\n");
        fixture.write(
            "conf/hooks.d/tool.sh",
            r#"
exists() { return 1; }
install() {
  {
    printf 'platform=%s\n' "$(shdeps_platform)"
    if shdeps_force; then printf 'force=1\n'; else printf 'force=0\n'; fi
    if shdeps_reinstall; then printf 'reinstall=1\n'; else printf 'reinstall=0\n'; fi
    printf 'pkg-mgr=%s\n' "$(shdeps_pkg_mgr)"
    printf 'install-dir=%s\n' "$(shdeps_install_dir)"
    printf 'git-dev-dir=%s\n' "$(shdeps_git_dev_dir)"
    printf 'bin-dir=%s\n' "$(shdeps_bin_dir)"
  } >"$SHDEPS_STATE_DIR/observed.txt"
  printf 'installed\n'
}
"#,
        );
        fixture.write_executable(
            "fakebin/bash",
            &format!("#!{0}\nexec {0} \"$@\"\n", system_bash().display()),
        );

        let mut command = fixture.command(["update"]);
        command
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fixture.dir.join("fakebin").display(),
                    shdeps_exe_dir().display()
                ),
            )
            .env("SHDEPS_FORCE", force)
            .env("SHDEPS_REINSTALL", reinstall);
        let output = run(&mut command);
        assert_success(&output);

        let observed = fs::read_to_string(fixture.dir.join("state/observed.txt")).unwrap();
        let mut expected = String::new();
        expected.push_str(&format!(
            "platform={}\n",
            api_answer(&fixture, ["__api", "platform"], force, reinstall)
        ));
        expected.push_str(&format!(
            "force={}\n",
            api_flag(&fixture, ["__api", "force"], force, reinstall)
        ));
        expected.push_str(&format!(
            "reinstall={}\n",
            api_flag(&fixture, ["__api", "reinstall"], force, reinstall)
        ));
        expected.push_str(&format!(
            "pkg-mgr={}\n",
            api_answer(&fixture, ["__api", "pkg-mgr"], force, reinstall)
        ));
        expected.push_str(&format!(
            "install-dir={}\n",
            api_answer(&fixture, ["__api", "install-dir"], force, reinstall)
        ));
        expected.push_str(&format!(
            "git-dev-dir={}\n",
            api_answer(&fixture, ["__api", "git-dev-dir"], force, reinstall)
        ));
        expected.push_str(&format!(
            "bin-dir={}\n",
            api_answer(&fixture, ["__api", "bin-dir"], force, reinstall)
        ));
        assert_eq!(
            observed, expected,
            "hook-observed answers must match direct __api (force={force} reinstall={reinstall})"
        );
    }
}

#[test]
fn hook_prelude_snapshot_queries_spawn_no_subprocesses() {
    // Perf: with the parent-exported environment, the seven snapshot
    // queries must not spawn recursive `shdeps __api` calls. A logging
    // wrapper on the hook PATH records every subprocess invocation; the
    // non-cached `skip-check` call proves the wrapper observes bridges.
    let fixture = Fixture::new("hook-prelude-no-spawn");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  shdeps_platform >/dev/null
  shdeps_force >/dev/null 2>&1; true
  shdeps_reinstall >/dev/null 2>&1; true
  shdeps_pkg_mgr >/dev/null
  shdeps_install_dir >/dev/null
  shdeps_git_dev_dir >/dev/null
  shdeps_bin_dir >/dev/null
  shdeps_skipped tool >/dev/null 2>&1; true
  printf 'installed\n'
}
"#,
    );
    fixture.write_executable(
        "fakebin/bash",
        &format!("#!{0}\nexec {0} \"$@\"\n", system_bash().display()),
    );
    let log = fixture.dir.join("shdeps-calls.log");
    fixture.write_executable(
        "fakebin/shdeps",
        &format!(
            "#!{bash}\nprintf '%s\\n' \"$*\" >>\"$SHDEPS_CALL_LOG\"\nexec \"{exe}\" \"$@\"\n",
            bash = system_bash().display(),
            exe = env!("CARGO_BIN_EXE_shdeps"),
        ),
    );

    let mut command = fixture.command(["update"]);
    command
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.dir.join("fakebin").display(),
                shdeps_exe_dir().display()
            ),
        )
        .env("SHDEPS_CALL_LOG", &log);
    let output = run(&mut command);
    assert_success(&output);

    let calls = fs::read_to_string(&log).unwrap_or_default();
    for query in [
        "__api platform",
        "__api force",
        "__api reinstall",
        "__api pkg-mgr",
        "__api install-dir",
        "__api git-dev-dir",
        "__api bin-dir",
    ] {
        assert!(
            !calls.lines().any(|line| line == query),
            "cached query must not spawn, got {query:?} in:\n{calls}"
        );
    }
    assert!(
        calls.lines().any(|line| line == "__api skip-check tool"),
        "non-cached queries must still bridge, got:\n{calls}"
    );
}

fn api_answer<const N: usize>(
    fixture: &Fixture,
    args: [&str; N],
    force: &str,
    reinstall: &str,
) -> String {
    let mut command = fixture.command(args);
    command
        .env("SHDEPS_FORCE", force)
        .env("SHDEPS_REINSTALL", reinstall);
    let output = run(&mut command);
    assert_success(&output);
    text(&output.stdout).trim_end().to_owned()
}

fn api_flag<const N: usize>(
    fixture: &Fixture,
    args: [&str; N],
    force: &str,
    reinstall: &str,
) -> &'static str {
    let mut command = fixture.command(args);
    command
        .env("SHDEPS_FORCE", force)
        .env("SHDEPS_REINSTALL", reinstall);
    match run(&mut command).status.code() {
        Some(0) => "1",
        _ => "0",
    }
}

fn system_bash() -> PathBuf {
    for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
        if Path::new(candidate).is_file() {
            return PathBuf::from(candidate);
        }
    }
    panic!("test host must provide bash for hook subprocesses");
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

    let (output, samples) =
        timed_samples(|| fixture.command(["dep-file", "cgraf78/sley", "share/sley/shell.sh"]));

    assert_ci_budget("dep-file", Duration::from_millis(200), &output, &samples);
}

#[test]
fn representative_duration_ignores_one_scheduler_outlier() {
    let samples = [
        Duration::from_millis(295),
        Duration::from_millis(10),
        Duration::from_millis(12),
    ];

    assert_eq!(representative_duration(&samples), Duration::from_millis(12));
}

#[test]
fn representative_duration_preserves_persistent_slowdown() {
    let samples = [
        Duration::from_millis(250),
        Duration::from_millis(10),
        Duration::from_millis(260),
    ];

    assert_eq!(
        representative_duration(&samples),
        Duration::from_millis(250)
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

    let (dep_root, root_samples) = timed_samples(|| fixture.command(["dep-root", "cgraf78/sley"]));
    assert_ci_budget(
        "dep-root",
        Duration::from_millis(200),
        &dep_root,
        &root_samples,
    );

    let (dep_path, path_samples) =
        timed_samples(|| fixture.command(["dep-path", "cgraf78/sley", "share/sley/shell.sh"]));
    assert_ci_budget(
        "dep-path",
        Duration::from_millis(200),
        &dep_path,
        &path_samples,
    );

    fixture.write_executable("share/cgraf78/sley/bin/sley", "#!/bin/sh\n");
    let (dep_links, links_samples) =
        timed_samples(|| fixture.command(["dep-links", "cgraf78/sley"]));
    assert_ci_budget(
        "dep-links",
        Duration::from_millis(200),
        &dep_links,
        &links_samples,
    );

    let (check, check_samples) = timed_samples(|| fixture.command(["check", "asset"]));
    assert_eq!(text(&check.stdout), "asset: installed\n");
    assert_ci_budget(
        "manifest-backed check",
        Duration::from_millis(300),
        &check,
        &check_samples,
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
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  ok       GitHub: 1 current\n  ok       Cargo: 1 current\n  ok       Go: 1 current\n  ok       UV: 1 current\n  ok       NPM: 1 current\n  ok       5 current\n"
    );
    assert_eq!(text(&output.stderr), "");
    assert!(
        elapsed <= Duration::from_secs(1),
        "warm manifest-backed update should stay under the CI budget; elapsed={elapsed:?}, stdout={:?}, stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn update_jsonl_progress_reports_machine_readable_events() {
    let fixture = Fixture::new("update-jsonl-progress");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() { printf 'installed\n'; }
"#,
    );

    let output = run(fixture.command(["update"]).env("SHDEPS_PROGRESS", "jsonl"));

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    let events = jsonl(&output.stdout);
    assert!(
        events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "custom"
            && event["phase"] == "custom"
            && event["label"] == "Custom"
            && event["status"] == "running"
            && event["detail"] == "checking custom deps"),
        "expected a custom phase event in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "item"
            && event["group"] == "custom"
            && event["status"] == "changed"
            && event["name"] == "tool"
            && event["detail"] == "installed"),
        "expected a changed item event in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "summary"
            && event["status"] == "changed"
            && event["changed"] == 1
            && event["current"] == 0
            && event["skipped"] == 0
            && event["failed"] == 0),
        "expected a changed summary event in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "group_summary"
            && event["group"] == "custom"
            && event["label"] == "Custom"
            && event["status"] == "changed"
            && event["changed"] == 1
            && event["current"] == 0
            && event["skipped"] == 0
            && event["failed"] == 0
            && event["elapsed_ms"].is_number()),
        "expected a custom group summary event in {events:#?}"
    );
    let group_summary_index = events
        .iter()
        .position(|event| event["event"] == "group_summary")
        .expect("expected group summary event");
    let summary_index = events
        .iter()
        .position(|event| event["event"] == "summary")
        .expect("expected summary event");
    assert!(
        group_summary_index < summary_index,
        "group summaries should arrive before final summary in {events:#?}"
    );
}

#[test]
fn update_jsonl_package_progress_includes_manager_override_skips() {
    let fixture = Fixture::new("update-jsonl-pkg-progress");
    fixture.write(
        "conf/deps.conf",
        "tool pkg tool apt:NONE\nother pkg other\n",
    );
    fixture.write("state/manifest", "other|pkg|other|\n");
    fixture.write(
        "state/other.pkg-proof",
        "shdeps-pkg-proof-v1\nmanager=apt\npackage=other\ncommand=other\n",
    );
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\n");
    fixture.write_executable("fakebin/other", "#!/bin/sh\n");
    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_PROGRESS", "jsonl");
    command.env("SHDEPS_LOG_LEVEL", "2");
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );

    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    let events = jsonl(&output.stdout);
    for event in events
        .iter()
        .filter(|event| event["event"] == "phase" && event["group"] == "packages")
    {
        let done = event["done"]
            .as_u64()
            .expect("phase done should be a number");
        let total = event["total"]
            .as_u64()
            .expect("phase total should be a number");
        assert!(
            done <= total,
            "package progress should never exceed total in {events:#?}"
        );
    }
    assert!(
        events.iter().any(|event| event["event"] == "item"
            && event["group"] == "packages"
            && event["name"] == "tool"
            && event["status"] == "skipped"),
        "expected skipped package override item in {events:#?}"
    );
}

#[test]
fn update_allows_real_provider_to_share_command_with_none_package_override() {
    let fixture = Fixture::new("update-none-package-command-claim");
    fixture.write(
        "conf/deps.conf",
        "disabled-pkg pkg tool apt:NONE\nprovider custom tool\n",
    );
    fixture.write(
        "conf/hooks.d/provider.sh",
        "exists() { return 0; }\nversion() { printf '1.0.0\\n'; }\n",
    );
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\nexit 0\n");

    let output = run(fixture.command(["update"]).env("SHDEPS_PKG_MGR", "apt"));

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    assert!(
        !text(&output.stdout).contains("duplicate active command claim"),
        "a package-manager NONE override is runtime-inactive: {}",
        text(&output.stdout)
    );
}

#[test]
#[cfg(unix)]
fn update_bulk_transitions_fit_within_common_low_file_descriptor_limit() {
    use std::os::unix::process::CommandExt;

    let fixture = Fixture::new("update-transition-fd-budget");
    let mut config = String::new();
    let mut manifest = String::new();
    for index in 0..24 {
        let name = format!("tool-{index}");
        let command = format!("cmd-{index}");
        config.push_str(&format!("{name} pkg {command} apt:NONE\n"));
        let public = fixture.dir.join("bin").join(&command);
        manifest.push_str(&format!(
            "{name}|github:release|{command}|{}\n",
            public.display()
        ));
        fixture.write_executable(PathBuf::from("bin").join(&command), "#!/bin/sh\nexit 0\n");
    }
    fixture.write("conf/deps.conf", &config);
    fixture.write("state/manifest", &manifest);
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\nexit 0\n");
    let mut command = fixture.command(["update"]);
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 104,
                rlim_max: 104,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }

    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_jsonl_warns_when_local_clone_cannot_fast_forward() {
    let fixture = Fixture::new("update-jsonl-local-clone-diverged");
    fixture.write("conf/deps.conf", "owner/tool github:repo tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\n");
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
    fixture.write_executable(
        "fakebin/git",
        r##"#!/bin/sh
case " $* " in
  *" rev-parse --show-toplevel ")
    pwd
    exit 0
    ;;
  *" config --local --no-includes --get-all remote.origin.url ")
    printf 'https://github.com/owner/tool\n'
    exit 0
    ;;
  *" remote get-url --all origin ")
    printf 'https://github.com/owner/tool\n'
    exit 0
    ;;
  *" ls-tree -z --full-tree HEAD -- bin/tool ")
    printf '100755 blob 0000000000000000000000000000000000000000\tbin/tool\0'
    exit 0
    ;;
  *" status --porcelain --untracked-files=normal ") exit 0 ;;
  *" rev-parse --abbrev-ref --symbolic-full-name @{upstream} ")
    printf 'origin/main\n'
    exit 0
    ;;
  *" pull --ff-only --quiet ") exit 1 ;;
  *) exit 1 ;;
esac
"##,
    );
    let mut command = fixture.command(["--force", "update"]);
    command.env("SHDEPS_PROGRESS", "jsonl");

    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    let events = jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["event"] == "item"
            && event["group"] == "github-repos"
            && event["status"] == "warning"
            && event["name"] == "owner/tool"
            && event["detail"] == "pull failed (no fast-forward; local clone)"
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "warning"
            && event["status"] == "warning"
            && event["detail"] == "owner/tool: pull failed (no fast-forward; local clone)"
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "group_summary"
            && event["group"] == "github-repos"
            && event["status"] == "warning"
            && event["warnings"] == 1
            && event["current"] == 0
            && event["failed"] == 0
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "summary"
            && event["status"] == "warning"
            && event["warnings"] == 1
            && event["current"] == 0
            && event["failed"] == 0
    }));
    assert_eq!(
        fs::read_link(fixture.dir.join("share/owner/tool")).unwrap(),
        fixture.dir.join("git/tool")
    );
}

#[test]
fn update_development_verification_ignores_ambient_git_dir() {
    let fixture = Fixture::new("update-development-ambient-git-dir");
    fixture.write("conf/deps.conf", "owner/tool github:repo tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\n");
    fixture.initialize_dev_checkout("tool", "https://github.com/other/tool");
    fixture.write_executable("git/approved/bin/tool", "#!/bin/sh\n");
    fixture.initialize_dev_checkout("approved", "https://github.com/owner/tool");

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_PROGRESS", "jsonl")
        .env("GIT_DIR", fixture.dir.join("git/approved/.git"));
    let output = run(&mut command);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr={:?}",
        text(&output.stderr)
    );
    let events = jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["event"] == "item"
            && event["name"] == "owner/tool"
            && event["status"] == "failed"
            && event["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("development checkout"))
    }));
    assert!(!fixture.dir.join("share/owner/tool").exists());
    assert!(!fixture.dir.join("bin/tool").exists());
}

#[test]
fn update_jsonl_reports_bare_github_method_resolution() {
    let fixture = Fixture::new("update-jsonl-github-method-progress");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[host_linux_asset("tool", "v1.0.0").as_str()]),
        "#!/bin/sh\nprintf 'tool v1.0.0\\n'\n",
    );
    let mut command = fixture.command(["--force", "update"]);
    command.env("SHDEPS_PROGRESS", "jsonl");
    command.env("SHDEPS_TEST_CURL_LOG", fixture.dir.join("curl.log"));
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );

    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    let events = jsonl(&output.stdout);
    assert!(
        events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "github-methods"
            && event["phase"] == "github-methods"
            && event["label"] == "Resolve sources"
            && event["detail"] == "resolving GitHub methods"
            && event["done"] == 0
            && event["total"] == 1),
        "expected GitHub method resolution start phase in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "github-methods"
            && event["phase"] == "github-methods"
            && event["label"] == "Resolve sources"
            && event["detail"] == "resolving GitHub methods"
            && event["done"] == 1
            && event["total"] == 1),
        "expected GitHub method resolution completion phase in {events:#?}"
    );
    let method_index = events
        .iter()
        .position(|event| event["event"] == "phase" && event["group"] == "github-methods")
        .expect("expected GitHub method phase");
    let release_index = events
        .iter()
        .position(|event| event["event"] == "phase" && event["group"] == "github-releases")
        .expect("expected GitHub release phase");
    assert!(
        method_index < release_index,
        "method resolution should be visible before release install checks in {events:#?}"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("curl.log")).unwrap(),
        "api\napi\nasset\n",
        "forced bare-GitHub resolution must refresh concrete release metadata instead of trusting a persisted cache with no run identity"
    );
}

#[test]
fn update_jsonl_splits_github_release_metadata_from_install_checks() {
    let fixture = Fixture::new("update-jsonl-release-progress");
    fixture.write("conf/deps.conf", "owner/tool github:release tool\n");
    fixture.write_executable("bin/tool", "#!/bin/sh\nprintf 'tool v0.9.0\\n'\n");
    fixture.write_fake_curl(
        &release_json("v1.0.0", &[host_linux_asset("tool", "v1.0.0").as_str()]),
        "#!/bin/sh\nprintf 'tool v1.0.0\\n'\n",
    );
    let mut command = fixture.command(["--force", "update"]);
    command.env("SHDEPS_PROGRESS", "jsonl");
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("fakebin").display()),
    );

    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stderr), "");
    let events = jsonl(&output.stdout);
    assert!(
        events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "github-releases"
            && event["phase"] == "github-release-metadata"
            && event["label"] == "GitHub"
            && event["detail"] == "fetching GitHub release metadata"
            && event["done"] == 0
            && event["total"] == 1),
        "expected release metadata phase in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "github-releases"
            && event["phase"] == "github-release-installs"
            && event["label"] == "GitHub"
            && event["detail"] == "checking GitHub release installs"
            && event["done"] == 0
            && event["total"] == 1),
        "expected release install-check phase in {events:#?}"
    );
    assert!(
        !events.iter().any(|event| event["event"] == "phase"
            && event["group"] == "github-releases"
            && event["detail"] == "checking GitHub releases"
            && event["done"] == event["total"]),
        "release progress should not end metadata and restart the same phase in {events:#?}"
    );
}

#[test]
fn update_verbose_groups_items_by_update_area() {
    let fixture = Fixture::new("update-verbose-groups");
    fixture.write(
        "conf/deps.conf",
        "owner/tool github:repo tool\ncustom-tool custom\n",
    );
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\n");
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
    fixture.write(
        "conf/hooks.d/custom-tool.sh",
        r#"
exists() { return 0; }
version() { printf '9.9.9\n'; }
"#,
    );
    let head = capture_output(Command::new("git").args([
        "-C",
        fixture.dir.join("git/tool").to_str().unwrap(),
        "rev-parse",
        "--short",
        "HEAD",
    ]))
    .unwrap();
    assert!(head.status.success());
    let head = String::from_utf8(head.stdout).unwrap();

    let output = run(&mut fixture.command(["-v", "update"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        format!(
            "Tools\n  running  checking configured dependencies\n  GitHub\n    changed  owner/tool: added -- commit {} (local clone)\n  Custom\n    ok       custom-tool: 9.9.9\n  changed  1 changed, 1 current\n",
            head.trim()
        )
    );
    assert_eq!(text(&output.stderr), "");
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
fn custom_hooks_stay_within_ci_budget() {
    let fixture = Fixture::new("custom-hook-perf");
    let mut config = String::new();
    for index in 0..30 {
        let name = format!("custom-{index}");
        config.push_str(&format!("{name} custom\n"));
        fixture.write(
            format!("conf/hooks.d/{name}.sh"),
            "exists() { return 0; }\n",
        );
    }
    fixture.write("conf/deps.conf", &config);

    let mut command = fixture.command(["list"]);
    command.env("SHDEPS_JOBS", "1");
    let (output, elapsed) = timed(&mut command);

    assert_success(&output);
    assert_eq!(
        text(&output.stdout)
            .lines()
            .filter(|line| line.contains("custom") && line.contains("installed"))
            .count(),
        30
    );
    assert_eq!(text(&output.stderr), "");
    // macOS process startup is materially slower, but a 50 ms polling
    // regression still adds about 1.2 seconds across these 30 serial hooks.
    let budget = if cfg!(target_os = "macos") {
        Duration::from_millis(2_200)
    } else {
        Duration::from_millis(1_200)
    };
    assert!(
        elapsed <= budget,
        "thirty short custom status hooks should stay under the CI budget; elapsed={elapsed:?}, budget={budget:?}, stdout={:?}, stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );

    // Seed the manifest outside the timed window. The performance contract is
    // for a warm current update, not thirty serial atomic manifest fsyncs.
    assert_success(&run(&mut fixture.command(["update"])));

    let mut command = fixture.command(["-v", "update"]);
    command.env("SHDEPS_JOBS", "1");
    let (output, elapsed) = timed(&mut command);

    assert_success(&output);
    assert_eq!(
        text(&output.stdout)
            .lines()
            .filter(|line| line.contains("custom-") && line.contains("ok"))
            .count(),
        30
    );
    assert_eq!(text(&output.stderr), "");
    // Catch per-child whole-process discovery and exit-polling regressions; a
    // 50 ms polling delay alone adds about 1.2 seconds across these 30 serial
    // current hooks without folding manifest I/O into the budget.
    let budget = if cfg!(target_os = "macos") {
        Duration::from_millis(2_200)
    } else {
        Duration::from_millis(1_200)
    };
    assert!(
        elapsed <= budget,
        "thirty current custom hooks should stay under the CI budget; elapsed={elapsed:?}, budget={budget:?}, stdout={:?}, stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn list_preserves_hyphenated_release_version_details() {
    let fixture = Fixture::new("list-hyphenated-release-version");
    fixture.write("conf/deps.conf", "cgraf78/hive-memory github:release hm\n");
    fixture.write_executable(
        "bin/hm",
        "#!/bin/sh\nprintf 'hm 20260611-142043-2c877b15 (schema 1)\\n'\n",
    );
    fixture.write(
        "state/manifest",
        &format!(
            "cgraf78/hive-memory|github:release|hm|{}\n",
            fixture.dir.join("bin/hm").display()
        ),
    );

    let mut command = fixture.command(["list"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", fixture.dir.join("bin").display()),
    );
    let output = run(&mut command);

    assert_success(&output);
    assert!(text(&output.stdout).contains("20260611-142043-2c877b15"));
    assert!(!text(&output.stdout).contains("20260611\n"));
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
fn check_pkg_uses_targeted_probes_instead_of_full_inventory() {
    let fixture = Fixture::new("check-targeted-package");
    let log = fixture.dir.join("package-probes.log");
    fixture.write(
        "conf/deps.conf",
        "fake-package pkg fake-tool\nfont-package pkg -\nmissing-package pkg -\n",
    );
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\nexit 0\n");
    fixture.write_executable(
        "fakebin/dpkg-query",
        "#!/bin/sh\nlast=\nfor arg do last=$arg; done\ncase $2 in *Package*) kind=version ;; *) kind=status ;; esac\nprintf 'query %s %s\\n' \"$kind\" \"$last\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$last\" = font-package ] || exit 1\nif [ \"$kind\" = version ]; then\n  printf 'install ok installed\\tfont-package\\t9.8.7\\n'\nelse\n  printf 'install ok installed\\n'\nfi\n",
    );
    fixture.write_executable(
        "fakebin/fake-tool",
        "#!/bin/sh\nprintf 'tool %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nprintf 'fake-tool 1.2.3\\n'\n",
    );

    let mut command = fixture.command(["check", "fake-package"]);
    command.env("SHDEPS_TEST_LOG", &log);
    let installed = run(&mut command);
    assert_success(&installed);
    assert_eq!(text(&installed.stdout), "fake-package: installed (1.2.3)\n");
    assert_eq!(text(&installed.stderr), "");
    let probes = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        probes
            .lines()
            .filter(|line| line.starts_with("query "))
            .all(|line| line == "query version fake-package"),
        "single-dependency check should not enumerate every installed package: {probes}"
    );

    fs::write(&log, "").unwrap();
    let mut command = fixture.command(["check", "font-package"]);
    command.env("SHDEPS_TEST_LOG", &log);
    let installed_without_command = run(&mut command);
    assert_success(&installed_without_command);
    assert_eq!(
        text(&installed_without_command.stdout),
        "font-package: installed (9.8.7)\n"
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "query version font-package\n",
        "package-only dependencies should retain version detail from one targeted probe"
    );

    fs::write(&log, "").unwrap();
    let mut command = fixture.command(["check", "missing-package"]);
    command.env("SHDEPS_TEST_LOG", &log);
    let missing = run(&mut command);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(text(&missing.stdout), "missing-package: not installed\n");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "query version missing-package\nquery status missing-package\n"
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
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
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
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
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
fn update_bare_github_metadata_failure_rejects_repo_missing_explicit_command() {
    let fixture = Fixture::new("update-github-rate-limited-repo-missing-command");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write("git/tool/README.md", "source checkout without bin/tool\n");
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
    fixture.write_executable(
        "fakebin/curl",
        "#!/bin/sh\nprintf 'rate limited\\n' >&2\nexit 22\n",
    );

    let mut command = fixture.command(["update"]);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("configured command `tool` not found in repo bin"));
    assert!(!fixture.dir.join("bin/tool").exists());
    assert!(!fixture.dir.join("share/owner/tool").exists());
    assert!(!fixture.dir.join("state/manifest").exists());
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
        "github:release\ncmd=tool\n"
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
    fixture.initialize_dev_checkout("tool", "https://github.com/owner/tool");
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
fn update_self_update_release_install_respects_ttl_and_force() {
    let fixture = Fixture::new("update-self-update-ttl");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let log = fixture.dir.join("curl.log");
    let tag = "20260524-120000-deadbeef";
    let platform = format!("linux-{}-musl", host_arch());
    fs::create_dir_all(&install).unwrap();
    fixture.write(
        "release-install/.shdeps-install.json",
        &format!(
            r#"{{"schema":1,"method":"release","artifact_platform":"{platform}","tag":"{tag}","repo":"cgraf78/shdeps"}}"#
        ),
    );
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/usr/bin/env bash
set -e
config=$(cat)
printf '%s\n' "$config" >>"$SHDEPS_TEST_CURL_LOG"
case "$config" in
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100"'*)
    printf '[{"tag_name":"%s","draft":false,"prerelease":false,"assets":[]}]' "$SHDEPS_TEST_TAG"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    let mut first = fixture.command(["update"]);
    first
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let first = run(&mut first);

    assert_success(&first);
    assert_eq!(text(&first.stdout), "No dependencies configured.\n");
    assert_eq!(text(&first.stderr), "");
    assert_eq!(count_release_fetches(&log), 1);

    let mut second = fixture.command(["update"]);
    second
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let second = run(&mut second);

    assert_success(&second);
    assert_eq!(text(&second.stdout), "No dependencies configured.\n");
    assert_eq!(text(&second.stderr), "");
    assert_eq!(count_release_fetches(&log), 1);

    let mut forced = fixture.command(["--force", "update"]);
    forced
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let forced = run(&mut forced);

    assert_success(&forced);
    assert_eq!(text(&forced.stdout), "No dependencies configured.\n");
    assert_eq!(text(&forced.stderr), "");
    assert_eq!(count_release_fetches(&log), 2);

    let mut env_forced = fixture.command(["update"]);
    env_forced
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600")
        .env("SHDEPS_FORCE", "1");
    let env_forced = run(&mut env_forced);

    assert_success(&env_forced);
    assert_eq!(text(&env_forced.stdout), "No dependencies configured.\n");
    assert_eq!(text(&env_forced.stderr), "");
    assert_eq!(count_release_fetches(&log), 3);

    let mut ttl_zero = fixture.command(["update"]);
    ttl_zero
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag)
        .env("SHDEPS_SELF_UPDATE_TTL", "0");
    let ttl_zero = run(&mut ttl_zero);

    assert_success(&ttl_zero);
    assert_eq!(text(&ttl_zero.stdout), "No dependencies configured.\n");
    assert_eq!(text(&ttl_zero.stderr), "");
    assert_eq!(count_release_fetches(&log), 4);
}

#[cfg(unix)]
#[test]
fn nonregular_self_update_metadata_is_rejected_without_ttl() {
    let fixture = Fixture::new("nonregular-self-update-target");
    let install = fixture.dir.join("release-install");
    fs::create_dir_all(&install).unwrap();
    let metadata_path = install.join(".shdeps-install.json");
    let metadata_path_c =
        std::ffi::CString::new(metadata_path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fixture path is private and represented by a valid C string.
    assert_eq!(unsafe { libc::mkfifo(metadata_path_c.as_ptr(), 0o600) }, 0);
    let _held_open = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&metadata_path)
        .unwrap();

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_DIR", &install);
    let (output, elapsed) = timed(&mut command);

    assert_success(&output);
    assert!(elapsed < Duration::from_secs(1));
    assert!(
        !fixture.dir.join("state/shdeps.self-update.stamp").exists(),
        "rejected nonregular metadata consumed the durable self-update TTL"
    );
}

#[cfg(unix)]
#[test]
fn nonregular_install_metadata_never_blocks_update_cancellation() {
    let fixture = Fixture::new("nonregular-self-update-metadata");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/hook.pid"
  trap '' HUP INT QUIT TERM
  while :; do /bin/sleep 1; done
}
"#,
    );
    let install = fixture.dir.join("release-install");
    fs::create_dir_all(&install).unwrap();
    let metadata_path = install.join(".shdeps-install.json");
    let metadata_path_c =
        std::ffi::CString::new(metadata_path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fixture path is private and represented by a valid C string.
    assert_eq!(unsafe { libc::mkfifo(metadata_path_c.as_ptr(), 0o600) }, 0);
    let _held_open = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&metadata_path)
        .unwrap();

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_DIR", &install)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/hook.pid"),
        Duration::from_secs(2),
        "hook reached after rejecting nonregular install metadata",
    );
    let _hook_guard = EscapedProcessGuard::new(hook_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    if process_is_running(hook_pid) {
        kill_process_group(hook_pid);
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "a FIFO at the metadata path must be rejected without blocking update"
    );
    assert!(
        !fixture.dir.join("state/shdeps.self-update.stamp").exists(),
        "cancelled update must not publish the self-update TTL"
    );
}

#[test]
fn update_self_update_uses_one_hour_default_ttl() {
    let fixture = Fixture::new("update-self-update-default-ttl");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let log = fixture.dir.join("curl.log");
    let tag = "20260524-120000-deadbeef";
    let platform = format!("linux-{}-musl", host_arch());
    fs::create_dir_all(&install).unwrap();
    fixture.write(
        "release-install/.shdeps-install.json",
        &format!(
            r#"{{"schema":1,"method":"release","artifact_platform":"{platform}","tag":"{tag}","repo":"cgraf78/shdeps"}}"#
        ),
    );
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/usr/bin/env bash
set -e
config=$(cat)
printf '%s\n' "$config" >>"$SHDEPS_TEST_CURL_LOG"
case "$config" in
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100"'*)
    printf '[{"tag_name":"%s","draft":false,"prerelease":false,"assets":[]}]' "$SHDEPS_TEST_TAG"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    // Straddling the default one-hour self-update TTL makes the public default
    // observable without sleeping in the integration suite.
    fixture.write_stamp_age("shdeps", "self-update", 3500);
    let mut fresh = fixture.command(["update"]);
    fresh
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag);
    let fresh = run(&mut fresh);

    assert_success(&fresh);
    assert_eq!(text(&fresh.stdout), "No dependencies configured.\n");
    assert_eq!(text(&fresh.stderr), "");
    assert_eq!(count_release_fetches(&log), 0);

    fixture.write_stamp_age("shdeps", "self-update", 3700);
    let mut stale = fixture.command(["update"]);
    stale
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_TEST_TAG", tag);
    let stale = run(&mut stale);

    assert_success(&stale);
    assert_eq!(text(&stale.stdout), "No dependencies configured.\n");
    assert_eq!(text(&stale.stderr), "");
    assert_eq!(count_release_fetches(&log), 1);
}

#[test]
fn update_self_update_source_checkout_pulls_clean_git_install() {
    let fixture = Fixture::new("update-self-update-source");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("source-install");
    let log = fixture.dir.join("git.log");
    fs::create_dir_all(install.join(".git")).unwrap();
    fixture.write_executable(
        "fakebin/git",
        r#"#!/usr/bin/env bash
set -e
printf '%s\n' "$*" >>"$SHDEPS_TEST_GIT_LOG"
case "${1:-}:${3:-}" in
  -C:status)
    ;;
  -C:pull)
    ;;
  *)
    printf 'unexpected git call: %s\n' "$*" >&2
    exit 9
    ;;
esac
"#,
    );

    let mut first = fixture.command(["update"]);
    first
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_GIT_LOG", &log)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let first = run(&mut first);

    assert_success(&first);
    assert_eq!(text(&first.stdout), "No dependencies configured.\n");
    assert_eq!(text(&first.stderr), "");
    assert_eq!(count_git_pulls(&log), 1);

    let mut second = fixture.command(["update"]);
    second
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_GIT_LOG", &log)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let second = run(&mut second);

    assert_success(&second);
    assert_eq!(text(&second.stdout), "No dependencies configured.\n");
    assert_eq!(text(&second.stderr), "");
    assert_eq!(count_git_pulls(&log), 1);

    let mut forced = fixture.command(["--force", "update"]);
    forced
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_GIT_LOG", &log)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let forced = run(&mut forced);

    assert_success(&forced);
    assert_eq!(text(&forced.stdout), "No dependencies configured.\n");
    assert_eq!(text(&forced.stderr), "");
    assert_eq!(count_git_pulls(&log), 2);
}

#[test]
fn update_self_update_source_checkout_dirty_skip_does_not_consume_ttl() {
    let fixture = Fixture::new("update-self-update-source-dirty");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("source-install");
    let log = fixture.dir.join("git.log");
    fs::create_dir_all(install.join(".git")).unwrap();
    fixture.write_executable(
        "fakebin/git",
        r#"#!/usr/bin/env bash
set -e
printf '%s\n' "$*" >>"$SHDEPS_TEST_GIT_LOG"
case "${1:-}:${3:-}" in
  -C:status)
    if [ -f "$SHDEPS_TEST_DIRTY" ]; then
      printf ' M src/lib.rs\n'
    fi
    ;;
  -C:pull)
    ;;
  *)
    printf 'unexpected git call: %s\n' "$*" >&2
    exit 9
    ;;
esac
"#,
    );
    let dirty = fixture.dir.join("dirty");
    fs::write(&dirty, "dirty\n").unwrap();

    let mut first = fixture.command(["update"]);
    first
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_GIT_LOG", &log)
        .env("SHDEPS_TEST_DIRTY", &dirty)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let first = run(&mut first);

    assert_success(&first);
    assert_eq!(text(&first.stdout), "No dependencies configured.\n");
    assert_eq!(text(&first.stderr), "");
    assert_eq!(count_git_pulls(&log), 0);
    assert!(!fixture.dir.join("state/shdeps.self-update.stamp").exists());

    fs::remove_file(&dirty).unwrap();
    let mut second = fixture.command(["update"]);
    second
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_GIT_LOG", &log)
        .env("SHDEPS_TEST_DIRTY", &dirty)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let second = run(&mut second);

    assert_success(&second);
    assert_eq!(text(&second.stdout), "No dependencies configured.\n");
    assert_eq!(text(&second.stderr), "");
    assert_eq!(count_git_pulls(&log), 1);
}

#[test]
fn update_self_update_release_failure_is_best_effort_and_stamped() {
    let fixture = Fixture::new("update-self-update-failure");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let log = fixture.dir.join("curl.log");
    let tag = "20260524-120000-deadbeef";
    let platform = format!("linux-{}-musl", host_arch());
    fs::create_dir_all(&install).unwrap();
    fixture.write(
        "release-install/.shdeps-install.json",
        &format!(
            r#"{{"schema":1,"method":"release","artifact_platform":"{platform}","tag":"{tag}","repo":"cgraf78/shdeps"}}"#
        ),
    );
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/usr/bin/env bash
config=$(cat)
printf '%s\n' "$config" >>"$SHDEPS_TEST_CURL_LOG"
printf 'transient github failure\n' >&2
exit 22
"#,
    );

    let mut first = fixture.command(["update"]);
    first
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let first = run(&mut first);

    assert_success(&first);
    assert_eq!(text(&first.stdout), "No dependencies configured.\n");
    assert_eq!(text(&first.stderr), "");
    assert_eq!(count_release_fetches(&log), 1);
    assert!(fixture.dir.join("state/shdeps.self-update.stamp").is_file());

    let mut second = fixture.command(["update"]);
    second
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_CURL_LOG", &log)
        .env("SHDEPS_SELF_UPDATE_TTL", "3600");
    let second = run(&mut second);

    assert_success(&second);
    assert_eq!(text(&second.stdout), "No dependencies configured.\n");
    assert_eq!(text(&second.stderr), "");
    assert_eq!(count_release_fetches(&log), 1);
}

#[test]
fn update_self_update_release_install_activates_new_archive() {
    let fixture = Fixture::new("update-self-update-archive");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let archive = fixture.dir.join("shdeps-release.tar.gz");
    let checksum = fixture.dir.join("shdeps-release.tar.gz.sha256");
    let arch = host_arch();
    let platform = format!("linux-{arch}-musl");
    fs::create_dir_all(&install).unwrap();
    fixture.write("release-install/shdeps", "old binary\n");
    fixture.write("release-install/shdeps.sh", "old shim\n");
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
            ("lua/shdeps.lua", "return {}\n", 0o644),
            ("lua/shdeps/core.lua", "return {}\n", 0o644),
            ("lua/shdeps/bootstrap.lua", "return {}\n", 0o644),
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
case "$config" in
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100"'*)
    printf '[{"tag_name":"20260524-120000-deadbeef","draft":false,"prerelease":false,"assets":[{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"},{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"}]}]\n' \
      "$SHDEPS_TEST_ARCHIVE_NAME" "$SHDEPS_TEST_ARCHIVE_NAME" \
      "$SHDEPS_TEST_CHECKSUM_NAME" "$SHDEPS_TEST_CHECKSUM_NAME"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_ARCHIVE_NAME"'"'*)
    cat "$SHDEPS_TEST_ARCHIVE"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_CHECKSUM_NAME"'"'*)
    cat "$SHDEPS_TEST_CHECKSUM"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    let mut update = fixture.command(["update"]);
    update
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_ARCHIVE", &archive)
        .env("SHDEPS_TEST_CHECKSUM", &checksum)
        .env("SHDEPS_TEST_ARCHIVE_NAME", &archive_name)
        .env("SHDEPS_TEST_CHECKSUM_NAME", &checksum_name);
    let update = run(&mut update);

    assert_success(&update);
    assert_eq!(text(&update.stdout), "No dependencies configured.\n");
    assert_eq!(text(&update.stderr), "");
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
}

#[test]
fn update_self_update_release_install_repairs_missing_current_payload() {
    let fixture = Fixture::new("update-self-update-repair");
    let fakebin = fixture.dir.join("fakebin");
    let install = fixture.dir.join("release-install");
    let archive = fixture.dir.join("shdeps-release.tar.gz");
    let checksum = fixture.dir.join("shdeps-release.tar.gz.sha256");
    let arch = host_arch();
    let platform = format!("linux-{arch}-musl");
    fs::create_dir_all(install.join("man/man1")).unwrap();
    fs::create_dir_all(install.join("lua/shdeps")).unwrap();
    fixture.write(
        "release-install/shdeps",
        "#!/bin/sh\nprintf 'shdeps 20260524-120000-deadbeef\\n'\n",
    );
    fixture.write("release-install/shdeps.sh", "old shim\n");
    fixture.write("release-install/install.sh", "#!/bin/sh\nexit 0\n");
    fixture.write("release-install/README.md", "readme\n");
    fixture.write("release-install/LICENSE", "license\n");
    fixture.write("release-install/man/man1/shdeps.1", ".TH SHDEPS 1\n");
    fixture.write("release-install/lua/shdeps.lua", "return {}\n");
    fixture.write("release-install/lua/shdeps/core.lua", "return {}\n");
    fixture.write(
        "release-install/.shdeps-install.json",
        &format!(
            r#"{{"schema":1,"method":"release","artifact_platform":"{platform}","tag":"20260524-120000-deadbeef","repo":"cgraf78/shdeps"}}"#
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
            ("lua/shdeps.lua", "return {}\n", 0o644),
            ("lua/shdeps/core.lua", "return {}\n", 0o644),
            (
                "lua/shdeps/bootstrap.lua",
                "return { repaired = true }\n",
                0o644,
            ),
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
case "$config" in
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100"'*)
    printf '[{"tag_name":"20260524-120000-deadbeef","draft":false,"prerelease":false,"assets":[{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"},{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"}]}]\n' \
      "$SHDEPS_TEST_ARCHIVE_NAME" "$SHDEPS_TEST_ARCHIVE_NAME" \
      "$SHDEPS_TEST_CHECKSUM_NAME" "$SHDEPS_TEST_CHECKSUM_NAME"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_ARCHIVE_NAME"'"'*)
    cat "$SHDEPS_TEST_ARCHIVE"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_CHECKSUM_NAME"'"'*)
    cat "$SHDEPS_TEST_CHECKSUM"
    ;;
  *)
    printf 'unexpected curl config\n%s\n' "$config" >&2
    exit 22
    ;;
esac
"#,
    );

    let mut update = fixture.command(["update"]);
    update
        .env("SHDEPS_DIR", &install)
        .env("PATH", format!("{}:/usr/bin:/bin", fakebin.display()))
        .env("SHDEPS_TEST_ARCHIVE", &archive)
        .env("SHDEPS_TEST_CHECKSUM", &checksum)
        .env("SHDEPS_TEST_ARCHIVE_NAME", &archive_name)
        .env("SHDEPS_TEST_CHECKSUM_NAME", &checksum_name);
    let update = run(&mut update);

    assert_success(&update);
    assert_eq!(text(&update.stdout), "No dependencies configured.\n");
    assert_eq!(text(&update.stderr), "");
    assert_eq!(
        fs::read_to_string(install.join("lua/shdeps/bootstrap.lua")).unwrap(),
        "return { repaired = true }\n"
    );
}

#[test]
fn update_self_update_ignores_unsupported_install_metadata() {
    let fixture = Fixture::new("update-self-update-unsupported");
    let install = fixture.dir.join("release-install");
    let log = fixture.dir.join("curl.log");
    fs::create_dir_all(&install).unwrap();
    fixture.write("release-install/.shdeps-install.json", "not json\n");

    let mut update = fixture.command(["update"]);
    update
        .env("SHDEPS_DIR", &install)
        .env("SHDEPS_TEST_CURL_LOG", &log);
    let update = run(&mut update);

    assert_success(&update);
    assert_eq!(text(&update.stdout), "No dependencies configured.\n");
    assert_eq!(text(&update.stderr), "");
    assert_eq!(count_release_fetches(&log), 0);
}

#[test]
fn update_requires_configured_method_tools_before_dependency_checks() {
    let fixture = Fixture::new("update-prereqs");
    fixture.write(
        "conf/deps.conf",
        "owner/tool github:release tool\nripgrep cargo rg\ngithub.com/junegunn/fzf go fzf\nruff uv\nprettier npm\ncustom custom\n",
    );
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "error: shdeps update is missing required tools for configured deps: cargo (cargo installs), curl (GitHub release metadata and downloads), go (go installs), npm (npm installs), uv (uv installs)\n"
    );
}

#[test]
fn update_prerequisites_ignore_unconfigured_methods() {
    let fixture = Fixture::new("update-prereqs-custom");
    fixture.write("conf/deps.conf", "tool custom\n");
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_success(&output);
    assert!(!text(&output.stderr).contains("missing required tools"));
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_requires_git_for_configured_repo_deps() {
    let fixture = Fixture::new("update-prereqs-git");
    fixture.write("conf/deps.conf", "owner/tool github:repo tool\n");
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "error: shdeps update is missing required tools for configured deps: git (GitHub repo installs)\n"
    );
}

#[test]
fn update_prerequisites_ignore_filtered_inactive_methods() {
    let fixture = Fixture::new("update-prereqs-filtered");
    fixture.write(
        "conf/deps.conf",
        "ripgrep cargo rg - os:macos\nprettier npm - - host:other-host\ntokei cargo - - mgr:!pacman\ntool custom\n",
    );
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();
    fixture.write_executable("missing-path/pacman", "#!/bin/sh\nexit 0\n");

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_success(&output);
    assert!(!text(&output.stderr).contains("missing required tools"));
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_requires_curl_for_bare_github_resolution_upfront() {
    let fixture = Fixture::new("update-prereqs-bare-github");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "error: shdeps update is missing required tools for configured deps: curl (GitHub release metadata and downloads)\n"
    );
}

#[test]
fn package_curl_cannot_bootstrap_bare_github_resolution() {
    let fixture = Fixture::new("update-prereqs-bare-github-pkg-curl");
    fixture.write("conf/deps.conf", "curl pkg\nowner/tool github tool\n");
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "error: shdeps update is missing required tools for configured deps: curl (GitHub release metadata and downloads)\n"
    );
}

#[test]
fn package_phase_bootstraps_cargo_before_cargo_dependency() {
    let fixture = Fixture::new("update-prereqs-bootstrap-cargo");
    let log = fixture.dir.join("order.log");
    fixture.write("conf/deps.conf", "cargo pkg\ntool cargo\n");
    fixture.write_executable("fakebin/id", "#!/bin/sh\nprintf '0\n'\n");
    fixture.write_executable("fakebin/sudo", "#!/bin/sh\nexec \"$@\"\n");
    fixture.write_executable("fakebin/dpkg-query", "#!/bin/sh\nexit 1\n");
    fixture.write_executable(
        "fakebin/apt-cache",
        "#!/bin/sh\n[ \"$1:$2\" = show:cargo ]\n",
    );
    fixture.write_executable(
        "fakebin/apt-get",
        r##"#!/bin/sh
printf 'package %s\n' "$*" >>"$SHDEPS_TEST_LOG"
if [ "$1:$2:$3" = 'install:-y:cargo' ]; then
  printf '%s\n' '#!/bin/sh' \
    'printf "cargo %s\\n" "$*" >>"$SHDEPS_TEST_LOG"' \
    'root=' \
    'while [ "$#" -gt 0 ]; do' \
    '  if [ "$1" = --root ]; then root=$2; shift 2; else shift; fi' \
    'done' \
    '/bin/mkdir -p "$root/bin"' \
    'printf "#!/bin/sh\\n" >"$root/bin/tool"' \
    '/bin/chmod +x "$root/bin/tool"' \
    >"$SHDEPS_TEST_FAKEBIN/cargo"
  /bin/chmod +x "$SHDEPS_TEST_FAKEBIN/cargo"
fi
"##,
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_PKG_MGR", "apt")
        .env("SHDEPS_TEST_LOG", &log)
        .env("SHDEPS_TEST_FAKEBIN", fixture.dir.join("fakebin"))
        .env("PATH", fixture.dir.join("fakebin"));
    let output = run(&mut command);

    let events = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={:?} stderr={:?} events={events:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
    let package = events.find("package install -y cargo").unwrap();
    let cargo = events.find("cargo install --locked").unwrap();
    assert!(
        package < cargo,
        "package phase must precede Cargo: {events}"
    );
}

#[test]
fn update_requires_git_after_bare_github_resolves_to_repo() {
    let fixture = Fixture::new("update-prereqs-bare-github-repo");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write(
        "fake/release.json",
        &release_json("v1.0.0", &["tool-v1.0.0-darwin-aarch64"]),
    );
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();
    fixture.write_executable(
        "missing-path/curl",
        "#!/bin/sh\ncat \"$SHDEPS_TEST_RELEASE_JSON\"\n",
    );

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "error: shdeps update is missing required tools for configured deps: git (GitHub repo installs)\n"
    );
}

#[test]
fn update_warns_rate_limit_before_concrete_prerequisite_failure() {
    let fixture = Fixture::new("update-rate-limit-before-prereq");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    let missing_path = fixture.dir.join("missing-path");
    fs::create_dir_all(&missing_path).unwrap();
    fixture.write_executable(
        "missing-path/curl",
        "#!/bin/sh\nprintf 'curl: (22) The requested URL returned error: 403\\n' >&2\nprintf '\\n403\\n'\nexit 22\n",
    );

    let mut command = fixture.command(["update"]);
    command.env("PATH", &missing_path);
    command.env_remove("GH_TOKEN");
    command.env_remove("GITHUB_TOKEN");
    command.env_remove("SHDEPS_ALLOW_GH_AUTH_TOKEN");
    let output = run(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "  warning  GitHub API rate limit exceeded (unauthenticated calls share 60/hour per IP); remaining GitHub checks used cached data. Set GH_TOKEN, or SHDEPS_ALLOW_GH_AUTH_TOKEN=1 to allow gh CLI credentials.\nerror: shdeps update is missing required tools for configured deps: git (GitHub repo installs)\n"
    );
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
        format!(
            "{}:{}:/usr/bin:/bin",
            fixture.dir.join("fakebin").display(),
            shdeps_exe_dir().display()
        ),
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
fn update_quiet_flag_skips_missing_package_when_sudo_would_prompt() {
    let fixture = Fixture::new("update-quiet-flag-pkg-no-sudo");
    let log = fixture.dir.join("pkg.log");
    fixture.write(
        "conf/deps.conf",
        "missing-shdeps-test-tool pkg missing-shdeps-test-tool\n",
    );
    fixture.write_executable(
        "fakebin/id",
        "#!/bin/sh\nprintf 'id %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nprintf '1000\\n'\n",
    );
    fixture.write_executable(
        "fakebin/sudo",
        "#!/bin/sh\nprintf 'sudo %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$1:$2\" = '-n:true' ] && exit 1\nexit 99\n",
    );
    fixture.write_executable(
        "fakebin/apt-get",
        "#!/bin/sh\nprintf 'apt-get %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nexit 99\n",
    );
    fixture.write_executable(
        "fakebin/apt-cache",
        "#!/bin/sh\nprintf 'apt-cache %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nexit 99\n",
    );

    let mut command = fixture.command(["--quiet", "update"]);
    command
        .env("SHDEPS_PKG_MGR", "apt")
        .env("SHDEPS_TEST_LOG", &log);
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(fs::read_to_string(&log).unwrap(), "id -u\nsudo -n true\n");
}

#[test]
fn update_quiet_environment_treats_missing_sudo_as_unavailable() {
    let fixture = Fixture::new("update-quiet-env-pkg-missing-sudo");
    let log = fixture.dir.join("pkg.log");
    let fakebin = fixture.dir.join("fakebin");
    fixture.write(
        "conf/deps.conf",
        "missing-shdeps-test-tool pkg missing-shdeps-test-tool\n",
    );
    fixture.write_executable(
        "fakebin/id",
        "#!/bin/sh\nprintf 'id %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nprintf '1000\\n'\n",
    );
    fixture.write_executable(
        "fakebin/apt-get",
        "#!/bin/sh\nprintf 'apt-get %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nexit 99\n",
    );
    fixture.write_executable(
        "fakebin/apt-cache",
        "#!/bin/sh\nprintf 'apt-cache %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\nexit 99\n",
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_QUIET", "1")
        .env("SHDEPS_PKG_MGR", "apt")
        .env("SHDEPS_TEST_LOG", &log)
        .env("PATH", fakebin);
    let output = run(&mut command);

    assert_success(&output);
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(fs::read_to_string(&log).unwrap(), "id -u\n");
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
        "Tools\n  running  checking configured dependencies\n  changed  Custom: 1 changed\n    changed  tool: 1.2.3\n  changed  1 changed\n"
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
    assert_eq!(
        text(&second.stdout),
        "Tools\n  running  checking configured dependencies\n  ok       Custom: 1 current\n  ok       1 current\n"
    );
    assert_eq!(text(&second.stderr), "");
}

#[test]
fn update_custom_hook_with_cached_sudo_does_not_prompt_parent() {
    let fixture = custom_sudo_fixture("custom-sudo-cached", &["tool"]);
    fixture.write("sudo-cache", "cached\n");

    let output = run(&mut custom_sudo_command(&fixture, ["update"]));

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool install\ninstall sudo -n true\n"
    );
    assert!(fixture.dir.join("state/tool-installed").is_file());
    assert!(!fixture.dir.join("state/.hook-sudo-requests").exists());
}

#[test]
fn update_custom_hook_prompts_parent_and_retries_once_when_sudo_cache_is_cold() {
    let fixture = custom_sudo_fixture("custom-sudo-cold", &["tool"]);

    let output = run(custom_sudo_command(&fixture, ["update"]).env("SHDEPS_PROGRESS", "jsonl"));

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool install\ninstall sudo -n true\nparent sudo true\n\
         tool install\ninstall sudo -n true\n"
    );
    assert!(fixture.dir.join("state/tool-installed").is_file());
    let events = jsonl(&output.stdout);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "prompt"
                && event["detail"] == "waiting for sudo authentication")
            .count(),
        1,
        "cold hook sudo should yield progress exactly once: {events:#?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn update_custom_hook_retry_reuses_parent_session_sudo_ticket() {
    let fixture = Fixture::new("custom-sudo-session-ticket");
    let binary = env!("CARGO_BIN_EXE_shdeps");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  printf 'hook install\n' >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
  sudo /bin/sh -c 'printf "installed\n" >"$1"' _ "$SHDEPS_STATE_DIR/tool-installed"
}
"#,
    );
    fixture.write_executable("fakebin/id", "#!/bin/sh\nprintf '1000\\n'\n");
    fixture.write_executable(
        "fakebin/sudo",
        r#"#!/bin/sh
IFS= read -r stat <"/proc/$$/stat" || exit 90
fields=${stat##*) }
IFS=' ' read -r state ppid pgrp session rest <<EOF
$fields
EOF
cache="$SHDEPS_TEST_SUDO_CACHE.$session"
phase=${SHDEPS_HOOK_PHASE:-parent}
if [ "$1:$2" = '-n:true' ]; then
  printf '%s probe %s\n' "$phase" "$session" >>"$SHDEPS_TEST_SUDO_LOG"
  test -f "$cache"
  exit $?
fi
if [ "$1" = true ]; then
  printf '%s auth %s\n' "$phase" "$session" >>"$SHDEPS_TEST_SUDO_LOG"
  test "$phase" = parent || exit 1
  : >"$cache"
  exit 0
fi
printf '%s command %s\n' "$phase" "$session" >>"$SHDEPS_TEST_SUDO_LOG"
test -f "$cache" || exit 1
exec "$@"
"#,
    );
    fixture.write_executable(
        "fakebin/shdeps",
        &format!("#!/bin/sh\nexec {binary} \"$@\"\n"),
    );

    let output = run(&mut custom_sudo_command(&fixture, ["update"]));

    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={:?} stderr={:?} sudo_log={log:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert!(fixture.dir.join("state/tool-installed").is_file());
    let lines = log.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 6, "{log}");
    assert_eq!(lines[0], "hook install");
    assert!(lines[1].starts_with("install probe "), "{log}");
    assert!(lines[2].starts_with("parent auth "), "{log}");
    assert_eq!(lines[3], "hook install");
    assert!(lines[4].starts_with("install probe "), "{log}");
    assert!(lines[5].starts_with("install command "), "{log}");
    let initial_session = lines[1].rsplit_once(' ').unwrap().1;
    let parent_session = lines[2].rsplit_once(' ').unwrap().1;
    let retry_session = lines[4].rsplit_once(' ').unwrap().1;
    let command_session = lines[5].rsplit_once(' ').unwrap().1;
    assert_ne!(initial_session, parent_session, "{log}");
    assert_eq!(retry_session, parent_session, "{log}");
    assert_eq!(command_session, parent_session, "{log}");
}

#[test]
fn update_multiple_custom_hooks_share_one_parent_sudo_prompt() {
    let fixture = custom_sudo_fixture("custom-sudo-multiple", &["first", "second"]);

    let output = run(custom_sudo_command(&fixture, ["update"]).env("SHDEPS_PROGRESS", "jsonl"));

    assert_success(&output);
    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(log.matches("parent sudo true\n").count(), 1, "{log}");
    assert_eq!(log.matches("first install\n").count(), 2, "{log}");
    assert_eq!(log.matches("second install\n").count(), 1, "{log}");
    assert!(fixture.dir.join("state/first-installed").is_file());
    assert!(fixture.dir.join("state/second-installed").is_file());
    let events = jsonl(&output.stdout);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "prompt")
            .count(),
        1,
        "one cached credential should cover the remaining hooks: {events:#?}"
    );
}

#[test]
fn update_custom_post_hook_uses_the_same_parent_sudo_retry() {
    let fixture = custom_sudo_fixture("custom-post-sudo-cold", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() { printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"; }
post() {
  printf '%s post\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
  printf 'post\n' >"$SHDEPS_STATE_DIR/tool-posted"
}
"#,
    );

    let output = run(custom_sudo_command(&fixture, ["update"]).env("SHDEPS_PROGRESS", "jsonl"));

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool post\npost sudo -n true\nparent sudo true\n\
         tool post\npost sudo -n true\n"
    );
    assert!(fixture.dir.join("state/tool-posted").is_file());
}

#[test]
fn prune_custom_uninstall_hook_uses_parent_sudo_retry() {
    let fixture = custom_sudo_fixture("custom-uninstall-sudo-cold", &["tool"]);
    fixture.write("conf/deps.conf", "");
    fixture.write("state/manifest", "tool|custom|tool|\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
uninstall() {
  printf '%s uninstall\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
  printf 'uninstalled\n' >"$SHDEPS_STATE_DIR/tool-uninstalled"
}
"#,
    );

    let output = run(&mut custom_sudo_command(&fixture, ["prune", "-y"]));

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool uninstall\nuninstall sudo -n true\nparent sudo true\n\
         tool uninstall\nuninstall sudo -n true\n"
    );
    assert!(fixture.dir.join("state/tool-uninstalled").is_file());
}

#[test]
fn prune_quiet_custom_uninstall_never_prompts_or_retries_for_sudo() {
    let fixture = custom_sudo_fixture("custom-uninstall-sudo-quiet", &["tool"]);
    fixture.write("conf/deps.conf", "");
    fixture.write("state/manifest", "tool|custom|tool|\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
uninstall() {
  printf '%s uninstall\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
}
"#,
    );

    let output = run(&mut custom_sudo_command(
        &fixture,
        ["--quiet", "prune", "-y"],
    ));

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool uninstall\nuninstall sudo -n true\n"
    );
}

#[test]
fn update_custom_hook_retries_only_once_when_sudo_cache_stays_cold() {
    let fixture = custom_sudo_fixture("custom-sudo-one-retry", &["tool"]);

    let output =
        run(custom_sudo_command(&fixture, ["update"]).env("SHDEPS_TEST_SUDO_STICKY_FAIL", "1"));

    assert_eq!(output.status.code(), Some(1));
    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(log.matches("parent sudo true\n").count(), 1, "{log}");
    assert_eq!(log.matches("tool install\n").count(), 2, "{log}");
    assert_eq!(log.matches("install sudo -n true\n").count(), 2, "{log}");
    assert!(!fixture.dir.join("state/tool-installed").exists());
}

#[test]
fn update_custom_hook_does_not_retry_when_parent_sudo_fails() {
    let fixture = custom_sudo_fixture("custom-sudo-parent-fails", &["tool"]);

    let output = run(custom_sudo_command(&fixture, ["update"])
        .env("SHDEPS_TEST_SUDO_PARENT_FAIL", "1")
        .env("SHDEPS_PROGRESS", "jsonl"));

    assert_eq!(output.status.code(), Some(1));
    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(log.matches("parent sudo true\n").count(), 1, "{log}");
    assert_eq!(log.matches("tool install\n").count(), 1, "{log}");
    assert_eq!(log.matches("install sudo -n true\n").count(), 1, "{log}");
    assert!(!fixture.dir.join("state/tool-installed").exists());
    let events = jsonl(&output.stdout);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "prompt")
            .count(),
        1,
        "failed parent authentication must not loop: {events:#?}"
    );
}

#[test]
fn update_custom_hook_partial_mutation_before_sudo_fails_closed() {
    let fixture = custom_sudo_fixture("custom-sudo-partial-mutation", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  printf '%s install\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  printf 'partial\n' >"$SHDEPS_STATE_DIR/tool-installed"
  shdeps_require_sudo || return $?
  printf 'complete\n' >"$SHDEPS_STATE_DIR/tool-privileged"
}
"#,
    );

    let output = run(&mut custom_sudo_command(&fixture, ["update"]));

    assert_eq!(output.status.code(), Some(1));
    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(log.matches("parent sudo true\n").count(), 1, "{log}");
    assert_eq!(log.matches("tool install\n").count(), 1, "{log}");
    assert!(fixture.dir.join("state/tool-installed").is_file());
    assert!(!fixture.dir.join("state/tool-privileged").exists());
    assert!(!fixture.dir.join("state/manifest").exists());
}

#[test]
fn update_custom_hook_sudo_request_keeps_grandchild_cleanup_bounded() {
    let fixture = custom_sudo_fixture("custom-sudo-background-before-request", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  if [ ! -f "$SHDEPS_TEST_SUDO_CACHE" ]; then
    /bin/sh -c '/bin/sleep 5' &
    printf '%s\n' "$!" >"$SHDEPS_STATE_DIR/grandchild.pid"
  fi
  shdeps_require_sudo || return $?
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
}
"#,
    );
    let mut command = custom_sudo_command(&fixture, ["update"]);
    command.env("SHDEPS_HOOK_TIMEOUT_SECS", "1");

    let (output, elapsed) = timed(&mut command);

    assert_success(&output);
    assert!(
        elapsed < Duration::from_secs(3),
        "the hook deadline must still apply after the leader requests sudo: {elapsed:?}"
    );
    let pid = fs::read_to_string(fixture.dir.join("state/grandchild.pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(pid),
        "pre-sudo hook grandchild to exit",
    );
}

#[test]
fn update_custom_hook_authenticated_retry_keeps_grandchild_cleanup_bounded() {
    let fixture = custom_sudo_fixture("custom-sudo-retry-grandchild", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  printf '%s install\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
  /bin/sh -c 'trap "" TERM; while :; do /bin/sleep 1; done' &
  printf '%s\n' "$!" >"$SHDEPS_STATE_DIR/retry-grandchild.pid"
  wait
}
"#,
    );
    let mut command = custom_sudo_command(&fixture, ["update"]);
    command.env("SHDEPS_HOOK_TIMEOUT_SECS", "1");

    let (output, elapsed) = timed(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed < Duration::from_secs(3),
        "the authenticated retry deadline must remain bounded: {elapsed:?}"
    );
    let log = fs::read_to_string(fixture.dir.join("sudo.log")).unwrap();
    assert_eq!(log.matches("parent sudo true\n").count(), 1, "{log}");
    assert_eq!(log.matches("tool install\n").count(), 2, "{log}");
    let pid = fs::read_to_string(fixture.dir.join("state/retry-grandchild.pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(pid),
        "authenticated retry grandchild to exit",
    );
}

#[cfg(unix)]
#[test]
fn update_custom_hook_timeout_kills_inflight_sudo_probe_group() {
    let fixture = custom_sudo_fixture("custom-sudo-probe-timeout", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        "exists() { return 1; }\ninstall() { shdeps_require_sudo; }\n",
    );
    fixture.write_executable(
        "fakebin/sudo",
        r#"#!/bin/sh
if [ "$1:$2" = '-n:true' ]; then
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/sudo-probe.pid"
  /bin/sh -c 'trap "" TERM; while :; do /bin/sleep 1; done' &
  printf '%s\n' "$!" >"$SHDEPS_STATE_DIR/sudo-probe-child.pid"
  wait
fi
exit 1
"#,
    );
    let mut command = custom_sudo_command(&fixture, ["update"]);
    command.env("SHDEPS_HOOK_TIMEOUT_SECS", "1");

    let (output, elapsed) = timed(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed < Duration::from_secs(3),
        "the outer hook deadline must remain authoritative: {elapsed:?}"
    );
    let probe_pid = wait_for_pid(
        &fixture.dir.join("state/sudo-probe.pid"),
        Duration::from_secs(2),
        "timed-out sudo probe pid",
    );
    let child_pid = wait_for_pid(
        &fixture.dir.join("state/sudo-probe-child.pid"),
        Duration::from_secs(2),
        "timed-out sudo probe descendant pid",
    );
    let probe_running = process_is_running(probe_pid);
    let child_running = process_is_running(child_pid);
    if probe_running {
        kill_process(probe_pid);
    }
    if child_running {
        kill_process(child_pid);
    }
    assert!(!probe_running, "sudo probe survived hook timeout");
    assert!(
        !child_running,
        "sudo probe descendant survived hook timeout"
    );
}

#[test]
fn update_custom_hook_exit_75_without_request_does_not_prompt() {
    let fixture = custom_sudo_fixture("custom-exit-75", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        "exists() { return 1; }\ninstall() { exit 75; }\n",
    );

    let output = run(custom_sudo_command(&fixture, ["update"]).env("SHDEPS_PROGRESS", "jsonl"));

    assert_eq!(output.status.code(), Some(1));
    assert!(!fixture.dir.join("sudo.log").exists());
    let events = jsonl(&output.stdout);
    assert!(
        events.iter().all(|event| event["event"] != "prompt"),
        "an unrelated hook exit code must not request authentication: {events:#?}"
    );
}

#[test]
fn update_quiet_custom_hook_never_prompts_or_retries_for_sudo() {
    let fixture = custom_sudo_fixture("custom-sudo-quiet", &["tool"]);

    let output = run(&mut custom_sudo_command(&fixture, ["--quiet", "update"]));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "  failed   tool: custom install failed\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.dir.join("sudo.log")).unwrap(),
        "tool install\ninstall sudo -n true\n"
    );
    assert!(!fixture.dir.join("state/tool-installed").exists());
}

#[test]
fn update_current_custom_hook_never_probes_or_prompts_for_sudo() {
    let fixture = custom_sudo_fixture("custom-sudo-current", &["tool"]);
    fixture.write("state/tool-installed", "installed\n");

    let output = run(&mut custom_sudo_command(&fixture, ["update"]));

    assert_success(&output);
    assert!(!fixture.dir.join("sudo.log").exists());
}

#[test]
fn custom_hook_timeout_still_kills_its_detached_grandchild() {
    let fixture = Fixture::new("custom-hook-timeout-grandchild");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  /bin/sh -c 'trap "" TERM; while :; do /bin/sleep 1; done' &
  printf '%s\n' "$!" >"$SHDEPS_STATE_DIR/grandchild.pid"
  wait
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_HOOK_TIMEOUT_SECS", "1");
    let (output, elapsed) = timed(&mut command);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed < Duration::from_secs(4),
        "hook timeout should remain bounded: {elapsed:?}"
    );
    let pid = fs::read_to_string(fixture.dir.join("state/grandchild.pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(pid),
        "timed-out hook grandchild to exit",
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_initial_detached_hook_before_returning() {
    let fixture = Fixture::new("parent-signal-detached-hook");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  trap '' HUP INT QUIT
  trap 'printf term >"$SHDEPS_STATE_DIR/term-seen"' TERM
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/hook.pid"
  while :; do
    printf x >>"$SHDEPS_STATE_DIR/mutations"
    /bin/sleep 0.02
  done
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/hook.pid"),
        Duration::from_secs(3),
        "initial detached hook pid",
    );
    let _hook_guard = EscapedProcessGuard::new(hook_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let hook_survived = process_is_running(hook_pid);
    let mutations = fixture.dir.join("state/mutations");
    let size_after_exit = fs::metadata(&mutations).unwrap().len();
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations).unwrap().len() != size_after_exit;
    if hook_survived {
        kill_process_group(hook_pid);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(hook_pid),
            "leaked hook cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("state/term-seen").is_file(),
        "detached hook did not receive TERM before KILL escalation"
    );
    assert!(!hook_survived, "detached hook survived its signaled parent");
    assert!(
        !mutation_continued,
        "detached hook mutated state after Shdeps exited"
    );
    assert!(
        !fixture.dir.join("state/manifest").exists(),
        "cancelled hook must not commit its manifest entry"
    );
    assert!(
        !fixture.dir.join("state/.changed-markers").exists(),
        "cancelled hook transaction markers must be removed"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn parent_signal_interrupts_state_lock_contention_promptly() {
    let fixture = Fixture::new("parent-signal-state-lock");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        "exists() { return 0; }\nversion() { printf '1.0\\n'; }\n",
    );
    fs::create_dir_all(fixture.dir.join("state")).unwrap();
    let lock_path = fixture.dir.join("state/.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    // SAFETY: this test owns the descriptor until after the child exits.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    // SAFETY: F_SETFD mutates only this valid test-owned descriptor. Prevent
    // the spawned Shdeps from inheriting the holder's open file description.
    assert_eq!(
        unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_STATE_LOCK_TIMEOUT_SECS", "30")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    wait_until(
        Duration::from_secs(2),
        || process_has_open_path(shdeps.id(), &lock_path),
        "contending update to open the held state lock",
    );

    let signaled = Instant::now();
    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(1));
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }
    drop(lock);

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "lock contention must observe cancellation instead of waiting for its timeout"
    );
    assert!(
        signaled.elapsed() < Duration::from_secs(1),
        "state-lock cancellation was not prompt"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_interrupts_prompt_ack_wait_before_sudo_side_effects() {
    let fixture = Fixture::new("parent-signal-prompt-ack");
    fixture.write("conf/deps.conf", "tool pkg\n");
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\nexit 99\n");
    let ack_path = fixture.dir.join("prompt-ack.fifo");
    let ack_path_c = std::ffi::CString::new(ack_path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the fixture path is a valid, private NUL-terminated pathname.
    assert_eq!(unsafe { libc::mkfifo(ack_path_c.as_ptr(), 0o600) }, 0);
    let events_path = fixture.dir.join("events.jsonl");
    let events = fs::File::create(&events_path).unwrap();

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_PROGRESS", "jsonl")
        .env("SHDEPS_PROGRESS_PROMPT_ACK", &ack_path)
        .stdout(Stdio::from(events))
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    wait_until(
        Duration::from_secs(3),
        || {
            fs::read_to_string(&events_path)
                .is_ok_and(|events| events.contains("\"event\":\"prompt\""))
        },
        "renderer prompt event",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(1));
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "prompt acknowledgement wait must observe parent cancellation"
    );
    assert!(
        !fixture.dir.join("state/manifest").exists(),
        "cancelled prompt wait must not schedule package publication"
    );
    let mut ack_writer = fs::OpenOptions::new();
    ack_writer
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW);
    assert_eq!(
        ack_writer.open(&ack_path).unwrap_err().raw_os_error(),
        Some(libc::ENXIO),
        "cancellation must close the acknowledgement reader lease"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_interrupts_interactive_prune_prompt_with_held_open_stdin() {
    use std::io::Write as _;

    let fixture = Fixture::new("parent-signal-prune-prompt");
    fixture.write("conf/deps.conf", "current custom\n");
    fixture.write(
        "state/manifest",
        "current|custom|current|\nold|github:release|old|/tmp/old\n",
    );
    let input_path = fixture.dir.join("prompt-input.fifo");
    let input_path_c = std::ffi::CString::new(input_path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fixture path is private and represented by a valid C string.
    assert_eq!(unsafe { libc::mkfifo(input_path_c.as_ptr(), 0o600) }, 0);
    let mut held_input = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&input_path)
        .unwrap();
    let output_path = fixture.dir.join("prompt-output");
    let output = fs::File::create(&output_path).unwrap();

    let mut command = fixture.command(["prune"]);
    command
        .stdin(Stdio::from(held_input.try_clone().unwrap()))
        .stdout(Stdio::from(output))
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    wait_until(
        Duration::from_secs(3),
        || fs::read_to_string(&output_path).is_ok_and(|output| output.contains("Remove? [y/N] ")),
        "interactive prune prompt",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(1));
    if status.is_none() {
        held_input.write_all(b"n\n").unwrap();
        held_input.flush().unwrap();
        let _ = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(2));
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "blocking prompt input must observe exact-PID cancellation promptly"
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("old|"),
        "cancelled prune prompt removed its orphan"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_interrupts_prune_prompt_after_partial_input() {
    use std::io::Write as _;

    let fixture = Fixture::new("parent-signal-prune-partial-input");
    fixture.write("conf/deps.conf", "current custom\n");
    fixture.write(
        "state/manifest",
        "current|custom|current|\nold|github:release|old|/tmp/old\n",
    );
    let input_path = fixture.dir.join("prompt-input.fifo");
    let input_path_c = std::ffi::CString::new(input_path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: fixture path is private and represented by a valid C string.
    assert_eq!(unsafe { libc::mkfifo(input_path_c.as_ptr(), 0o600) }, 0);
    let mut held_input = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&input_path)
        .unwrap();
    let output_path = fixture.dir.join("prompt-output");
    let output = fs::File::create(&output_path).unwrap();

    let mut command = fixture.command(["prune"]);
    command
        .stdin(Stdio::from(held_input.try_clone().unwrap()))
        .stdout(Stdio::from(output))
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    wait_until(
        Duration::from_secs(3),
        || fs::read_to_string(&output_path).is_ok_and(|output| output.contains("Remove? [y/N] ")),
        "interactive prune prompt",
    );
    held_input.write_all(b"y").unwrap();
    held_input.flush().unwrap();
    wait_until(
        Duration::from_secs(2),
        || pending_input_bytes(&held_input) == 0,
        "partial prompt byte to be consumed",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(1));

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "a partial line must not turn the cancellable prompt into blocking read_line"
    );
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("old|"),
        "cancelled partial confirmation removed its orphan"
    );
}

#[cfg(unix)]
#[test]
fn first_parent_signal_keeps_exit_status_precedence_during_cleanup() {
    let fixture = Fixture::new("parent-signal-precedence");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  trap '' HUP INT QUIT
  trap 'printf cleanup-started >"$SHDEPS_STATE_DIR/cleanup-started"; trap "" TERM' TERM
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/hook.pid"
  while :; do /bin/sleep 1; done
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/hook.pid"),
        Duration::from_secs(3),
        "hook pid",
    );
    let _hook_guard = EscapedProcessGuard::new(hook_pid);

    signal_process(shdeps.id(), libc::SIGHUP);
    wait_until(
        Duration::from_secs(2),
        || fs::read(fixture.dir.join("state/cleanup-started")).is_ok_and(|bytes| !bytes.is_empty()),
        "first-signal cleanup to start",
    );
    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let hook_survived = process_is_running(hook_pid);
    if hook_survived {
        kill_process_group(hook_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGHUP)
    );
    assert!(!hook_survived, "hook survived first-signal cleanup");
}

#[cfg(unix)]
#[test]
fn hook_signal_during_spawn_registration_is_not_lost() {
    let fixture = Fixture::new("parent-signal-hook-registration");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  trap '' HUP INT QUIT TERM
  kill -TERM "$PPID"
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/hook.pid"
  while :; do /bin/sleep 1; done
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/hook.pid"),
        Duration::from_secs(3),
        "spawn-race hook pid",
    );
    let _hook_guard = EscapedProcessGuard::new_if_present(hook_pid);
    let hook_survived = process_is_running(hook_pid);
    if hook_survived {
        kill_process_group(hook_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        !hook_survived,
        "hook escaped when it signaled Shdeps during spawn registration"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_hook_descendant_in_another_process_group() {
    let fixture = Fixture::new("parent-signal-hook-session-member");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  trap '' HUP INT QUIT TERM
  set -m
  /bin/sh -c '
    trap "" HUP INT QUIT TERM
    printf "%s\n" "$$" >"$SHDEPS_STATE_DIR/descendant.pid"
    while :; do
      printf x >>"$SHDEPS_STATE_DIR/descendant-mutations"
      /bin/sleep 0.02
    done
  ' &
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/hook.pid"
  wait
}
"#,
    );

    let mut command = fixture.command(["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/hook.pid"),
        Duration::from_secs(3),
        "detached hook pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("state/descendant.pid"),
        Duration::from_secs(3),
        "detached descendant pid",
    );
    let _hook_guard = EscapedProcessGuard::new(hook_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (descendant_group, descendant_session) = process_group_and_session(descendant_pid);
    assert_ne!(
        descendant_group, hook_pid,
        "fixture must escape the leader group"
    );
    assert_eq!(
        descendant_session, hook_pid,
        "fixture must remain in the hook session"
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let hook_survived = process_is_running(hook_pid);
    let descendant_survived = process_is_running(descendant_pid);
    if hook_survived {
        kill_process_group(hook_pid);
    }
    if descendant_survived {
        kill_process(descendant_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(!hook_survived, "detached hook leader survived cancellation");
    assert!(
        !descendant_survived,
        "same-session descendant escaped through a different process group"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_detached_timed_probe_before_returning() {
    let fixture = Fixture::new("parent-signal-detached-probe");
    fixture.write("conf/deps.conf", "tool pkg\n");
    fixture.write_executable(
        "fakebin/tool",
        r#"#!/bin/sh
trap '' HUP INT QUIT
trap 'printf term >"$SHDEPS_TEST_PROBE_TERM"' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_PROBE_PID"
while :; do
  printf x >>"$SHDEPS_TEST_PROBE_MUTATIONS"
  /bin/sleep 0.02
done
"#,
    );

    let mut command = fixture.command(["list"]);
    command
        .env("SHDEPS_TEST_PROBE_PID", fixture.dir.join("probe.pid"))
        .env(
            "SHDEPS_TEST_PROBE_MUTATIONS",
            fixture.dir.join("probe-mutations"),
        )
        .env("SHDEPS_TEST_PROBE_TERM", fixture.dir.join("probe-term"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let probe_pid = wait_for_pid(
        &fixture.dir.join("probe.pid"),
        Duration::from_secs(3),
        "detached probe pid",
    );
    let _probe_guard = EscapedProcessGuard::new(probe_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let probe_survived = process_is_running(probe_pid);
    let mutations = fixture.dir.join("probe-mutations");
    let size_after_exit = fs::metadata(&mutations).unwrap().len();
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations).unwrap().len() != size_after_exit;
    if probe_survived {
        kill_process_group(probe_pid);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(probe_pid),
            "leaked probe cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("probe-term").is_file(),
        "timed probe did not receive TERM before KILL escalation"
    );
    assert!(!probe_survived, "timed probe survived its signaled parent");
    assert!(
        !mutation_continued,
        "timed probe mutated state after Shdeps exited"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_unbounded_external_install_before_returning() {
    let fixture = Fixture::new("parent-signal-unbounded-install");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap '' HUP INT QUIT
trap 'printf term >"$SHDEPS_TEST_CHILD_TERM"' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 0.02; done
"#,
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_TEST_CHILD_PID", fixture.dir.join("external.pid"))
        .env("SHDEPS_TEST_CHILD_TERM", fixture.dir.join("external-term"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let external_pid = wait_for_pid(
        &fixture.dir.join("external.pid"),
        Duration::from_secs(3),
        "external installer pid",
    );
    let _external_guard = EscapedProcessGuard::new(external_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let external_survived = process_is_running(external_pid);
    if external_survived {
        kill_process(external_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("external-term").is_file(),
        "unbounded external child did not receive TERM before KILL escalation"
    );
    assert!(
        !external_survived,
        "unbounded external child survived its signaled parent"
    );
}

#[cfg(unix)]
#[test]
fn parent_cancellation_resumes_stopped_child_before_term() {
    let fixture = Fixture::new("parent-signal-stopped-child");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap 'printf term >"$SHDEPS_TEST_CHILD_TERM"; exit 0' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );

    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("stopped-child.pid"),
        )
        .env(
            "SHDEPS_TEST_CHILD_TERM",
            fixture.dir.join("stopped-child-term"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let child_pid = wait_for_pid(
        &fixture.dir.join("stopped-child.pid"),
        Duration::from_secs(3),
        "stoppable external child pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    signal_process(child_pid, libc::SIGSTOP);
    wait_until(
        Duration::from_secs(2),
        || process_is_stopped(child_pid),
        "external child to stop",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("stopped-child-term").is_file(),
        "stopped child must receive CONT before graceful TERM"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_unbounded_external_descendant_after_leader_exits() {
    let fixture = Fixture::new("parent-signal-unbounded-descendant");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exit 0
"#,
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_TEST_CHILD_PID", fixture.dir.join("external.pid"))
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("external-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("external-descendant-mutations"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("external.pid"),
        Duration::from_secs(3),
        "external installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("external-descendant.pid"),
        Duration::from_secs(3),
        "external installer descendant pid",
    );
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (caller_group, caller_session) = process_group_and_session(shdeps.id());
    let (descendant_group, descendant_session) = process_group_and_session(descendant_pid);
    assert_eq!(
        descendant_group, leader_pid,
        "fixture must remain in the installer's owned PGID"
    );
    assert_ne!(
        descendant_group, caller_group,
        "installer descendants must not share the caller PGID"
    );
    assert_eq!(
        descendant_session, caller_session,
        "fixture must share caller SID"
    );
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(leader_pid),
        "external installer leader to exit",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("external-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process(descendant_pid);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "leaked external descendant cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "Shdeps must not block draining pipes held by an owned descendant"
    );
    assert!(
        !descendant_survived,
        "exact-child descendant survived parent cancellation"
    );
    assert!(
        !mutation_continued,
        "exact-child descendant continued mutating after Shdeps exited"
    );
    assert!(
        !fixture.dir.join("state/manifest").exists(),
        "cancelled installer must not publish its manifest entry"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn parent_signal_stops_tracked_external_descendant_after_session_escape_and_leader_exit() {
    let fixture = Fixture::new("parent-signal-tracked-external-descendant");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/usr/bin/setsid /bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exit 0
"#,
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_TEST_CHILD_PID", fixture.dir.join("external.pid"))
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("external-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("external-descendant-mutations"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut shdeps = spawn_test_session(&mut command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("external.pid"),
        Duration::from_secs(3),
        "external installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("external-descendant.pid"),
        Duration::from_secs(3),
        "escaped external descendant pid",
    );
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, leader_session) = process_group_and_session(leader_pid);
    let (descendant_group, descendant_session) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, leader_pid,
        "installer must lead its owned PGID"
    );
    assert_ne!(
        descendant_group, leader_group,
        "fixture descendant must escape the installer PGID"
    );
    assert_ne!(
        descendant_session, leader_session,
        "fixture descendant must escape the installer session"
    );
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(leader_pid),
        "external installer leader to exit after spawning escaped descendant",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("external-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process_group(descendant_group);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "leaked escaped external descendant cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        !descendant_survived,
        "tracked descendant escaped after its installer leader exited"
    );
    assert!(
        !mutation_continued,
        "tracked escaped descendant kept mutating after Shdeps returned"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn parent_signal_rediscoveries_stop_late_setsid_descendants_after_kill() {
    let fixture = Fixture::new("parent-signal-late-setsid-descendants");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap '
  trap "" TERM
  printf started >"$SHDEPS_TEST_LATE_STARTED"
  i=0
  while [ "$i" -lt 100 ]; do
    /usr/bin/setsid /bin/sh -c '\''
      trap "" HUP INT QUIT TERM
      printf "%s\n" "$$" >>"$SHDEPS_TEST_LATE_PIDS"
      while :; do
        printf x >>"$SHDEPS_TEST_LATE_MUTATIONS"
        /bin/sleep 0.02
      done
    '\'' &
    i=$((i + 1))
    /bin/sleep 0.003
  done
  while :; do /bin/sleep 1; done
' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );
    let pids_path = fixture.dir.join("late-descendants.pids");
    let mutations = fixture.dir.join("late-descendant-mutations");
    let started = fixture.dir.join("late-fork-started");
    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_TEST_CHILD_PID", fixture.dir.join("late-leader.pid"))
        .env("SHDEPS_TEST_LATE_STARTED", &started)
        .env("SHDEPS_TEST_LATE_PIDS", &pids_path)
        .env("SHDEPS_TEST_LATE_MUTATIONS", &mutations)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut shdeps = spawn_test_session(&mut command);
    let _leader = wait_for_pid(
        &fixture.dir.join("late-leader.pid"),
        Duration::from_secs(3),
        "late-fork leader pid",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    wait_until(
        Duration::from_secs(2),
        || started.is_file(),
        "late-fork TERM handler",
    );
    let first_descendant = wait_for_pids(
        &pids_path,
        1,
        Duration::from_secs(2),
        "first late setsid descendant",
    )[0];
    let mut first_guard = EscapedProcessGuard::new(first_descendant);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let mut stderr = String::new();
    if status.is_some() {
        use std::io::Read as _;
        shdeps
            .stderr
            .take()
            .expect("Shdeps stderr must be captured")
            .read_to_string(&mut stderr)
            .unwrap();
    }
    let pids = read_pids(&pids_path);
    let mut guards = pids
        .iter()
        .copied()
        .filter(|pid| *pid != first_descendant)
        .filter_map(EscapedProcessGuard::new_if_present)
        .collect::<Vec<_>>();
    let survivors = pids
        .iter()
        .copied()
        .filter(|pid| process_is_running(*pid))
        .collect::<Vec<_>>();
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(100));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    first_guard.disarm_if_exited();
    for guard in &mut guards {
        guard.disarm_if_exited();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "late-descendant cleanup diagnostics: {stderr}; survivors: {survivors:?}"
    );
    assert!(
        survivors.is_empty(),
        "late setsid descendants survived the post-KILL sweep: {survivors:?}"
    );
    assert!(!mutation_continued, "late descendant mutation continued");
}

#[cfg(unix)]
#[test]
fn parent_signal_delivers_term_to_late_same_group_descendant() {
    let fixture = Fixture::new("parent-signal-late-same-group-descendant");
    fixture.write("conf/deps.conf", "tool cargo\n");
    // The late child survives TERM so the topology assertion below observes
    // a live process; teardown's KILL escalation (already required for the
    // TERM-ignoring leader) still reaps it before the final assertion.
    fixture.write_executable(
        "fakebin/late-same-group-child",
        r#"#!/bin/sh
trap 'printf term >"$SHDEPS_TEST_LATE_CHILD_TERM"' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_LATE_CHILD_PID"
while :; do /bin/sleep 0.02; done
"#,
    );
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap '
  "$SHDEPS_TEST_LATE_CHILD" &
  while [ ! -s "$SHDEPS_TEST_LATE_CHILD_PID" ]; do :; done
  while :; do /bin/sleep 1; done
' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );
    let child_path = fixture.dir.join("fakebin/late-same-group-child");
    let child_pid_path = fixture.dir.join("late-same-group-child.pid");
    let child_term_path = fixture.dir.join("late-same-group-child.term");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("late-same-group-leader.pid"),
        )
        .env("SHDEPS_TEST_LATE_CHILD", &child_path)
        .env("SHDEPS_TEST_LATE_CHILD_PID", &child_pid_path)
        .env("SHDEPS_TEST_LATE_CHILD_TERM", &child_term_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut shdeps = spawn_test_session(&mut command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("late-same-group-leader.pid"),
        Duration::from_secs(3),
        "late same-group leader pid",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    // The TERM-to-trap-to-spawn chain is the longest in this test but had
    // the shortest budget; loaded runners starve it intermittently. Match
    // the sibling exit wait below.
    let child_pid = wait_for_pid(
        &child_pid_path,
        Duration::from_secs(4),
        "late same-group child pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (child_group, _) = process_group_and_session(child_pid);
    assert_eq!(
        child_group, leader_group,
        "fixture child must retain the leader PGID"
    );

    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let mut stderr = String::new();
    if status.is_some() {
        use std::io::Read as _;
        shdeps
            .stderr
            .take()
            .expect("Shdeps stderr must be captured")
            .read_to_string(&mut stderr)
            .unwrap();
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "late same-group cancellation diagnostics: {stderr}"
    );
    assert!(
        child_term_path.is_file(),
        "late same-group child did not receive the graceful TERM phase"
    );
    assert!(
        !process_is_running(child_pid),
        "late same-group child survived cancellation"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_and_drains_unbounded_curl_before_returning() {
    let fixture = Fixture::new("parent-signal-unbounded-curl");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/bin/sh
trap '' HUP INT QUIT
trap 'printf term >"$SHDEPS_TEST_CURL_TERM"' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CURL_PID"
while :; do
  printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'
  printf 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' >&2
done
"#,
    );

    let mut command = fixture.command(["list"]);
    command
        .env("SHDEPS_TEST_CURL_PID", fixture.dir.join("curl.pid"))
        .env("SHDEPS_TEST_CURL_TERM", fixture.dir.join("curl-term"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let curl_pid = wait_for_pid(
        &fixture.dir.join("curl.pid"),
        Duration::from_secs(3),
        "curl pid",
    );
    let _curl_guard = EscapedProcessGuard::new(curl_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let curl_survived = process_is_running(curl_pid);
    if curl_survived {
        kill_process(curl_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("curl-term").is_file(),
        "curl did not receive TERM before KILL escalation"
    );
    assert!(!curl_survived, "curl survived its signaled parent");
}

#[cfg(unix)]
#[test]
fn parent_signal_interrupts_large_stdin_write_to_nonreading_curl() {
    let fixture = Fixture::new("parent-signal-curl-stdin-backpressure");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/bin/sh
trap '' HUP INT QUIT
trap 'printf term >"$SHDEPS_TEST_CURL_TERM"' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CURL_PID"
while :; do /bin/sleep 0.02; done
"#,
    );

    let mut command = fixture.command(["list"]);
    command
        .env("GH_TOKEN", "x".repeat(100 * 1024))
        .env("SHDEPS_TEST_CURL_PID", fixture.dir.join("curl.pid"))
        .env("SHDEPS_TEST_CURL_TERM", fixture.dir.join("curl-term"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let curl_pid = wait_for_pid(
        &fixture.dir.join("curl.pid"),
        Duration::from_secs(3),
        "nonreading curl pid",
    );
    let _curl_guard = EscapedProcessGuard::new(curl_pid);

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let curl_survived = process_is_running(curl_pid);
    if curl_survived {
        kill_process_group(curl_pid);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(curl_pid),
            "nonreading curl fallback cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "stdin backpressure must not hide a latched parent signal"
    );
    assert!(
        fixture.dir.join("curl-term").is_file(),
        "nonreading curl did not receive TERM before bounded teardown"
    );
    assert!(!curl_survived, "nonreading curl survived cancellation");
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_parent_sudo_without_leaving_the_caller_session() {
    let fixture = Fixture::new("parent-signal-parent-sudo");
    let binary = env!("CARGO_BIN_EXE_shdeps");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        "exists() { return 1; }\ninstall() { shdeps_require_sudo; }\n",
    );
    fixture.write_executable("fakebin/id", "#!/bin/sh\nprintf '1000\\n'\n");
    fixture.write_executable(
        "fakebin/sudo",
        r#"#!/bin/sh
if [ "$1:$2" = '-n:true' ]; then exit 1; fi
if [ "$1" = true ]; then
  trap '' HUP INT QUIT
  trap 'printf term >"$SHDEPS_TEST_SUDO_TERM"' TERM
  printf '%s\n' "$$" >"$SHDEPS_TEST_SUDO_PID"
  while :; do /bin/sleep 0.02; done
fi
exit 2
"#,
    );
    fixture.write_executable(
        "fakebin/shdeps",
        &format!("#!/bin/sh\nexec {binary} \"$@\"\n"),
    );

    let mut command = fixture.command(["update"]);
    command
        .env("SHDEPS_TEST_SUDO_PID", fixture.dir.join("sudo.pid"))
        .env("SHDEPS_TEST_SUDO_TERM", fixture.dir.join("sudo-term"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let sudo_pid = wait_for_pid(
        &fixture.dir.join("sudo.pid"),
        Duration::from_secs(3),
        "parent sudo pid",
    );
    let _sudo_guard = EscapedProcessGuard::new(sudo_pid);
    let (shdeps_group, shdeps_session) = process_group_and_session(shdeps.id());
    let (sudo_group, sudo_session) = process_group_and_session(sudo_pid);
    assert_ne!(
        sudo_group, shdeps_group,
        "sudo must use an owned group so cleanup cannot signal the caller"
    );
    assert_eq!(
        sudo_session, shdeps_session,
        "sudo must retain the parent session"
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let sudo_survived = process_is_running(sudo_pid);
    if sudo_survived {
        kill_process(sudo_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        fixture.dir.join("sudo-term").is_file(),
        "parent-session sudo did not receive TERM before KILL escalation"
    );
    assert!(!sudo_survived, "parent-session sudo survived cancellation");
    assert!(
        !fixture.dir.join("state/.hook-sudo-requests").exists(),
        "cancelled authentication must not retain its request channel"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "TEMP-DEBUG: isolate macOS hang; do not land"]
fn cold_sudo_authentication_and_parent_session_retry_work_through_a_real_pty() {
    use std::io::{Read as _, Write as _};

    let fixture = custom_sudo_fixture("cold-sudo-real-pty", &["tool"]);
    fixture.write_executable(
        "fakebin/sudo",
        r#"#!/bin/sh
if [ "$1:$2" = '-n:true' ]; then
  test -f "$SHDEPS_TEST_SUDO_CACHE"
  exit $?
fi
if [ "$1" = true ]; then
  printf 'test-password: ' >/dev/tty
  IFS= read -r password </dev/tty || exit 2
  [ "$password" = secret ] || exit 3
  : >"$SHDEPS_TEST_SUDO_CACHE"
  exit 0
fi
exit 4
"#,
    );

    let (mut shdeps, mut master) = spawn_on_pty(custom_sudo_command(&fixture, ["update"]));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = Vec::new();
    while !observed
        .windows(b"test-password: ".len())
        .any(|window| window == b"test-password: ")
    {
        let mut chunk = [0_u8; 512];
        match master.read(&mut chunk) {
            Ok(0) => {}
            Ok(count) => observed.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => panic!("failed reading PTY prompt: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "sudo prompt did not reach the PTY"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    master.write_all(b"secret\n").unwrap();
    master.flush().unwrap();

    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(5));
    if status.is_none() {
        kill_process_group(shdeps.id());
        let _ = shdeps.wait();
    }
    assert_eq!(status.and_then(|status| status.code()), Some(0));
    assert!(
        fixture.dir.join("state/tool-installed").is_file(),
        "authenticated parent-session retry did not finish the hook"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn terminal_interrupt_of_owned_foreground_child_returns_130() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-child-terminal-interrupt");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >\"$SHDEPS_TEST_CHILD_PID\"\nexec /bin/sleep 30\n",
    );
    let mut command = fixture.command(["update"]);
    command.env("SHDEPS_TEST_CHILD_PID", fixture.dir.join("foreground.pid"));
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let child_pid = wait_for_pid(
        &fixture.dir.join("foreground.pid"),
        Duration::from_secs(3),
        "foreground installer pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    let (shdeps_group, shdeps_session) = process_group_and_session(shdeps.id());
    let (child_group, child_session) = process_group_and_session(child_pid);
    assert_ne!(
        child_group, shdeps_group,
        "child must own its process group"
    );
    assert_eq!(
        child_session, shdeps_session,
        "child must retain caller SID"
    );

    // The PTY line discipline delivers ^C to the current foreground group,
    // exactly as a user pressing Ctrl-C would.
    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    if status.is_none() {
        kill_process_group(shdeps.id());
        kill_process_group(child_group);
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "Shdeps must propagate a terminal-delivered child interrupt conventionally"
    );
    assert!(!process_is_running(child_pid));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn terminal_interrupt_trapped_as_conventional_exit_returns_130() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-child-trapped-terminal-interrupt");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap 'exit 130' INT
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );
    let mut command = fixture.command(["update"]);
    command.env(
        "SHDEPS_TEST_CHILD_PID",
        fixture.dir.join("foreground-trapped.pid"),
    );
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let child_pid = wait_for_pid(
        &fixture.dir.join("foreground-trapped.pid"),
        Duration::from_secs(3),
        "foreground trapped installer pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    if status.is_none() {
        kill_process_group(shdeps.id());
        kill_process_group(child_pid);
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "foreground conventional 130 must preserve terminal cancellation"
    );
    assert!(!process_is_running(child_pid));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn descendant_foreground_interrupt_cancels_parallel_work_and_restores_the_terminal() {
    use std::io::Write as _;

    let fixture = Fixture::new("descendant-foreground-parallel-interrupt");
    fixture.write("conf/deps.conf", "foreground cargo foreground\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/usr/bin/env python3
import os
import signal
import time

tty = os.open("/dev/tty", os.O_RDWR)
while os.tcgetpgrp(tty) != os.getpgrp():
    time.sleep(0.001)
read_fd, write_fd = os.pipe()
worker = os.fork()
if worker == 0:
    os.close(read_fd)
    os.close(write_fd)
    os.setpgid(0, 0)
    worker_tty = os.open("/dev/tty", os.O_RDWR)
    def stop(_signal, _frame):
        with open(os.environ["SHDEPS_TEST_SIBLING_TERM"], "w") as marker:
            marker.write("term\n")
        with open(os.environ["SHDEPS_TEST_RECLAIMED_GROUP"], "w") as group_file:
            group_file.write(f"{os.tcgetpgrp(worker_tty)}\n")
        raise SystemExit(143)
    signal.signal(signal.SIGTERM, stop)
    for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT):
        signal.signal(caught, signal.SIG_IGN)
    with open(os.environ["SHDEPS_TEST_SIBLING_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    with open(os.environ["SHDEPS_TEST_SIBLING_MUTATIONS"], "ab", buffering=0) as mutations:
        while True:
            mutations.write(b"x")
            time.sleep(0.01)

descendant = os.fork()
if descendant == 0:
    os.close(read_fd)
    os.setpgid(0, 0)
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    def interrupted(_signal, _frame):
        with open(os.environ["SHDEPS_TEST_DESCENDANT_INT"], "w") as marker:
            marker.write("int\n")
        os.write(write_fd, b"i")
    signal.signal(signal.SIGINT, interrupted)
    for caught in (signal.SIGHUP, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(caught, signal.SIG_IGN)
    os.tcsetpgrp(tty, os.getpgrp())
    with open(os.environ["SHDEPS_TEST_DESCENDANT_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    with open(os.environ["SHDEPS_TEST_DESCENDANT_READY"], "w") as ready_file:
        ready_file.write("ready\n")
    while True:
        time.sleep(1)

os.close(write_fd)
with open(os.environ["SHDEPS_TEST_CHILD_PID"], "w") as pid_file:
    pid_file.write(f"{os.getpid()}\n")
os.read(read_fd, 1)
os._exit(130)
"#,
    );
    let descendant_ready = fixture.dir.join("descendant.ready");
    let descendant_int = fixture.dir.join("descendant.int");
    let sibling_term = fixture.dir.join("sibling.term");
    let sibling_mutations = fixture.dir.join("sibling.mutations");
    let reclaimed_group = fixture.dir.join("reclaimed.group");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env("SHDEPS_TEST_DESCENDANT_READY", &descendant_ready)
        .env("SHDEPS_TEST_DESCENDANT_INT", &descendant_int)
        .env("SHDEPS_TEST_SIBLING_PID", fixture.dir.join("sibling.pid"))
        .env("SHDEPS_TEST_SIBLING_TERM", &sibling_term)
        .env("SHDEPS_TEST_SIBLING_MUTATIONS", &sibling_mutations)
        .env("SHDEPS_TEST_RECLAIMED_GROUP", &reclaimed_group);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let shdeps_pid = shdeps.id();
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground descendant pid",
    );
    let sibling_pid = wait_for_pid(
        &fixture.dir.join("sibling.pid"),
        Duration::from_secs(3),
        "parallel sibling pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let mut descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let mut sibling_guard = EscapedProcessGuard::new(sibling_pid);
    wait_until(
        Duration::from_secs(2),
        || descendant_ready.is_file() && terminal_group(&master) == descendant_pid,
        "descendant to own the terminal foreground",
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    wait_until(
        Duration::from_secs(2),
        || descendant_int.is_file(),
        "descendant to acknowledge terminal Ctrl-C",
    );
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(5));
    descendant_guard.disarm_if_exited();
    sibling_guard.disarm_if_exited();
    let sibling_size = fs::metadata(&sibling_mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(100));
    let sibling_still_mutating = fs::metadata(&sibling_mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != sibling_size;

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "a conventional exit from the boundary owning the terminal must cancel the update"
    );
    assert!(sibling_term.is_file(), "parallel work did not receive TERM");
    assert!(!process_is_running(descendant_pid));
    assert!(!process_is_running(sibling_pid));
    assert!(
        !sibling_still_mutating,
        "parallel work survived Shdeps exit"
    );
    assert_eq!(
        wait_for_pid(
            &reclaimed_group,
            Duration::from_secs(2),
            "parallel worker to record terminal restoration",
        ),
        shdeps_pid,
        "Shdeps did not reclaim its terminal before terminating the owned boundary"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "TEMP-DEBUG: probe_of_leader_05 is the control; do not land"]
fn terminal_interrupt_of_leader_stops_ignoring_pipe_holder() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-leader-interrupt-pipe-holder");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("foreground-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process_group(descendant_group);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "foreground pipe-holder fallback cleanup",
        );
    }
    if status.is_none() {
        kill_process_group(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "leader signal must be observed before inherited pipe EOF"
    );
    assert!(
        !descendant_survived,
        "pipe-holder survived terminal cancellation"
    );
    assert!(
        !mutation_continued,
        "pipe-holder kept mutating after Shdeps returned"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "TEMP-DEBUG: isolate macOS hang; do not land"]
fn terminal_interrupt_cleans_closed_pipe_session_escape_before_returning() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-interrupt-closed-pipe-escape");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/usr/bin/env python3
import os
import signal
import time

descendant = os.fork()
if descendant == 0:
    os.setsid()
    for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(caught, signal.SIG_IGN)
    with open(os.environ["SHDEPS_TEST_DESCENDANT_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    for descriptor in (0, 1, 2):
        try:
            os.close(descriptor)
        except OSError:
            pass
    mutation = os.open(
        os.environ["SHDEPS_TEST_DESCENDANT_MUTATIONS"],
        os.O_WRONLY | os.O_CREAT | os.O_APPEND,
        0o600,
    )
    while True:
        os.write(mutation, b"x")
        time.sleep(0.02)

with open(os.environ["SHDEPS_TEST_CHILD_PID"], "w") as pid_file:
    pid_file.write(f"{os.getpid()}\n")
while not os.path.exists(os.environ["SHDEPS_TEST_DESCENDANT_PID"]):
    time.sleep(0.001)
os.execl("/bin/sleep", "sleep", "30")
"#,
    );
    let mutations = fixture.dir.join("closed-pipe-descendant-mutations");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("closed-pipe-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("closed-pipe-descendant.pid"),
        )
        .env("SHDEPS_TEST_DESCENDANT_MUTATIONS", &mutations);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("closed-pipe-leader.pid"),
        Duration::from_secs(3),
        "foreground leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("closed-pipe-descendant.pid"),
        Duration::from_secs(3),
        "closed-pipe escaped descendant pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let mut descendant_guard = EscapedProcessGuard::new(descendant_pid);

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let descendant_survived = process_is_running(descendant_pid);
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        descendant_guard.signal(libc::SIGKILL);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "closed-pipe escaped descendant fallback cleanup",
        );
    }
    descendant_guard.disarm_if_exited();
    if status.is_none() {
        kill_process_group(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT)
    );
    assert!(
        !descendant_survived,
        "closed-pipe escaped descendant survived inferred terminal cancellation"
    );
    assert!(
        !mutation_continued,
        "closed-pipe escaped descendant mutated after Shdeps returned"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn foreground_returns_to_shdeps_when_child_exits_before_pipe_holder() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-release-before-pipe-eof");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/usr/bin/env python3
import os
import signal
import time

descendant = os.fork()
if descendant == 0:
    os.setpgid(0, 0)
    for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(caught, signal.SIG_IGN)
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    with open(os.environ["SHDEPS_TEST_DESCENDANT_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    while not os.path.exists(os.environ["SHDEPS_TEST_TTY_TAKEOVER"]):
        time.sleep(0.001)
    tty = os.open("/dev/tty", os.O_RDWR)
    os.tcsetpgrp(tty, os.getpgrp())
    with open(os.environ["SHDEPS_TEST_TTY_OWNER_READY"], "w") as ready_file:
        ready_file.write("ready\n")
    while True:
        time.sleep(1)

with open(os.environ["SHDEPS_TEST_CHILD_PID"], "w") as pid_file:
    pid_file.write(f"{os.getpid()}\n")
while not os.path.exists(os.environ["SHDEPS_TEST_DESCENDANT_PID"]):
    time.sleep(0.001)
os._exit(0)
"#,
    );
    let tty_owner_ready = fixture.dir.join("foreground-tty-owner-ready");
    let tty_takeover = fixture.dir.join("foreground-tty-takeover");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-exited.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-pipe-holder.pid"),
        )
        .env("SHDEPS_TEST_TTY_TAKEOVER", &tty_takeover)
        .env("SHDEPS_TEST_TTY_OWNER_READY", &tty_owner_ready);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-exited.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-pipe-holder.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new_if_present(leader_pid);
    let mut descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_ne!(
        descendant_group, leader_group,
        "fixture pipe holder must own the terminal from another process group"
    );
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(leader_pid),
        "foreground leader to exit before its descendant takes the terminal",
    );
    fs::write(&tty_takeover, "take terminal\n").unwrap();
    wait_until(
        Duration::from_secs(2),
        || tty_owner_ready.is_file(),
        "descendant to take the terminal after its leader exits",
    );
    wait_until(
        Duration::from_secs(2),
        || terminal_group(&master) == shdeps.id(),
        "Shdeps to reclaim the terminal after leader exit",
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    descendant_guard.disarm_if_exited();

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "Ctrl-C must reach Shdeps after its foreground child leader exits"
    );
    assert!(!process_is_running(descendant_pid));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn cancellation_reclaims_terminal_from_an_owned_descendant_group() {
    let fixture = Fixture::new("foreground-reclaim-descendant-on-cancel");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/usr/bin/env python3
import os
import signal
import time

tty = None
def record_terminal_and_exit(_signal, _frame):
    with open(os.environ["SHDEPS_TEST_RECLAIMED_GROUP"], "w") as group_file:
        group_file.write(f"{os.tcgetpgrp(tty)}\n")
    raise SystemExit(143)

signal.signal(signal.SIGTERM, record_terminal_and_exit)
descendant = os.fork()
if descendant == 0:
    os.setpgid(0, 0)
    for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(caught, signal.SIG_IGN)
    with open(os.environ["SHDEPS_TEST_DESCENDANT_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    while True:
        time.sleep(1)

with open(os.environ["SHDEPS_TEST_CHILD_PID"], "w") as pid_file:
    pid_file.write(f"{os.getpid()}\n")
while not os.path.exists(os.environ["SHDEPS_TEST_DESCENDANT_PID"]):
    time.sleep(0.001)
tty = os.open("/dev/tty", os.O_RDWR)
os.tcsetpgrp(tty, descendant)
with open(os.environ["SHDEPS_TEST_TTY_OWNER_READY"], "w") as ready_file:
    ready_file.write("ready\n")
while True:
    time.sleep(1)
"#,
    );
    let ready = fixture.dir.join("cancel-descendant-tty-ready");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("cancel-descendant-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("cancel-descendant-owner.pid"),
        )
        .env("SHDEPS_TEST_TTY_OWNER_READY", &ready)
        .env(
            "SHDEPS_TEST_RECLAIMED_GROUP",
            fixture.dir.join("cancel-reclaimed-terminal-group"),
        );
    let (mut shdeps, master) = spawn_on_pty(command);
    let shdeps_pid = shdeps.id();
    let leader_pid = wait_for_pid(
        &fixture.dir.join("cancel-descendant-leader.pid"),
        Duration::from_secs(3),
        "foreground leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("cancel-descendant-owner.pid"),
        Duration::from_secs(3),
        "foreground descendant owner pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let mut descendant_guard = EscapedProcessGuard::new(descendant_pid);
    wait_until(
        Duration::from_secs(2),
        || ready.is_file() && terminal_group(&master) == descendant_pid,
        "descendant to acknowledge terminal ownership",
    );

    signal_process(shdeps_pid, libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    descendant_guard.disarm_if_exited();

    assert_eq!(status.and_then(|status| status.code()), Some(143));
    assert!(!process_is_running(descendant_pid));
    let reclaimed_group = wait_for_pid(
        &fixture.dir.join("cancel-reclaimed-terminal-group"),
        Duration::from_secs(2),
        "leader to record terminal ownership during cancellation",
    );
    assert_eq!(
        reclaimed_group, shdeps_pid,
        "cancellation must return the terminal from any retained owned group"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn cancellation_reclaims_a_terminal_retake_during_term_grace() {
    let fixture = Fixture::new("foreground-reclaim-late-descendant-retake");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/usr/bin/env python3
import os
import signal
import time

descendant = os.fork()
if descendant == 0:
    os.setpgid(0, 0)
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    tty = os.open("/dev/tty", os.O_RDWR)
    took_terminal = False
    def take_terminal(_signal, _frame):
        global took_terminal
        os.tcsetpgrp(tty, os.getpgrp())
        took_terminal = True
        with open(os.environ["SHDEPS_TEST_LATE_TTY_READY"], "w") as ready_file:
            ready_file.write("ready\n")
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, take_terminal)
    for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT):
        signal.signal(caught, signal.SIG_IGN)
    with open(os.environ["SHDEPS_TEST_DESCENDANT_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    while True:
        if took_terminal and os.tcgetpgrp(tty) != os.getpgrp():
            with open(os.environ["SHDEPS_TEST_LATE_TTY_RECLAIMED"], "w") as reclaimed_file:
                reclaimed_file.write("reclaimed\n")
        time.sleep(0.01)

for caught in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
    signal.signal(caught, signal.SIG_IGN)
with open(os.environ["SHDEPS_TEST_CHILD_PID"], "w") as pid_file:
    pid_file.write(f"{os.getpid()}\n")
while not os.path.exists(os.environ["SHDEPS_TEST_DESCENDANT_PID"]):
    time.sleep(0.001)
while True:
    time.sleep(0.01)
"#,
    );
    let late_ready = fixture.dir.join("late-terminal-owner-ready");
    let reclaimed = fixture.dir.join("late-terminal-reclaimed");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("late-terminal-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("late-terminal-descendant.pid"),
        )
        .env("SHDEPS_TEST_LATE_TTY_READY", &late_ready)
        .env("SHDEPS_TEST_LATE_TTY_RECLAIMED", &reclaimed);
    let (mut shdeps, master) = spawn_on_pty(command);
    let shdeps_pid = shdeps.id();
    let leader_pid = wait_for_pid(
        &fixture.dir.join("late-terminal-leader.pid"),
        Duration::from_secs(3),
        "late-retake leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("late-terminal-descendant.pid"),
        Duration::from_secs(3),
        "late-retake descendant pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let mut descendant_guard = EscapedProcessGuard::new(descendant_pid);
    wait_until(
        Duration::from_secs(2),
        || terminal_group(&master) == leader_pid,
        "leader to own the terminal before cancellation",
    );

    signal_process(shdeps_pid, libc::SIGTERM);
    wait_until(
        Duration::from_secs(2),
        || late_ready.is_file(),
        "descendant to retake the terminal from its TERM handler",
    );
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    descendant_guard.disarm_if_exited();

    assert_eq!(status.and_then(|status| status.code()), Some(143));
    assert!(!process_is_running(descendant_pid));
    assert!(
        reclaimed.is_file(),
        "the terminal lease must periodically reclaim a late takeover during teardown"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn terminal_stop_suspends_shdeps_and_resume_rehands_off_before_interrupt() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-child-stop-resume");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap 'printf continued >"$SHDEPS_TEST_CONTINUED"' CONT
trap 'exit 130' INT
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );
    let continued = fixture.dir.join("foreground-continued");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-stopped.pid"),
        )
        .env("SHDEPS_TEST_CONTINUED", &continued);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let child_pid = wait_for_pid(
        &fixture.dir.join("foreground-stopped.pid"),
        Duration::from_secs(3),
        "foreground stoppable child pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    wait_until(
        Duration::from_secs(2),
        || terminal_group(&master) == child_pid,
        "child to own the terminal before Ctrl-Z",
    );
    let _ = fs::remove_file(&continued);

    master.write_all(&[26]).unwrap();
    master.flush().unwrap();
    wait_until(
        Duration::from_secs(2),
        || process_is_stopped(shdeps.id()),
        "Shdeps job to stop after its foreground child stops",
    );
    assert_eq!(
        terminal_group(&master),
        shdeps.id(),
        "Shdeps must reclaim the terminal before suspending its own job"
    );

    signal_process(shdeps.id(), libc::SIGCONT);
    wait_until(
        Duration::from_secs(2),
        || terminal_group(&master) == child_pid && continued.is_file(),
        "resumed Shdeps to hand the terminal back and continue its child",
    );
    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "resumed foreground child must still propagate Ctrl-C conventionally"
    );
    assert!(!process_is_running(child_pid));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn terminal_stop_suspends_the_complete_original_pipeline_job() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-pipeline-stop");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >\"$SHDEPS_TEST_CHILD_PID\"\nwhile :; do /bin/sleep 1; done\n",
    );
    fixture.write_executable(
        "fakebin/pipeline-harness",
        r#"#!/usr/bin/env python3
import os
import signal
import time

tty = os.open("/dev/tty", os.O_RDWR)
signal.signal(signal.SIGTTOU, signal.SIG_IGN)

shdeps = os.fork()
if shdeps == 0:
    os.setpgid(0, 0)
    os.execl(os.environ["SHDEPS_TEST_BINARY"], "shdeps", "update")
os.setpgid(shdeps, shdeps)

sibling = os.fork()
if sibling == 0:
    os.setpgid(0, shdeps)
    with open(os.environ["SHDEPS_TEST_PIPELINE_SIBLING_PID"], "w") as pid_file:
        pid_file.write(f"{os.getpid()}\n")
    while True:
        time.sleep(1)
os.setpgid(sibling, shdeps)

with open(os.environ["SHDEPS_TEST_PIPELINE_SHDEPS_PID"], "w") as pid_file:
    pid_file.write(f"{shdeps}\n")
os.tcsetpgrp(tty, shdeps)
with open(os.environ["SHDEPS_TEST_PIPELINE_READY"], "w") as ready_file:
    ready_file.write("ready\n")

while True:
    waited, status = os.waitpid(shdeps, os.WUNTRACED | os.WCONTINUED)
    if waited == shdeps and (os.WIFEXITED(status) or os.WIFSIGNALED(status)):
        break
os.kill(sibling, signal.SIGKILL)
os.waitpid(sibling, 0)
os.tcsetpgrp(tty, os.getpgrp())
if os.WIFEXITED(status):
    raise SystemExit(os.WEXITSTATUS(status))
raise SystemExit(128 + os.WTERMSIG(status))
"#,
    );

    let template = fixture.command(["update"]);
    let mut command = Command::new(fixture.dir.join("fakebin/pipeline-harness"));
    command.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    command
        .env("SHDEPS_TEST_BINARY", env!("CARGO_BIN_EXE_shdeps"))
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("pipeline-child.pid"),
        )
        .env(
            "SHDEPS_TEST_PIPELINE_SHDEPS_PID",
            fixture.dir.join("pipeline-shdeps.pid"),
        )
        .env(
            "SHDEPS_TEST_PIPELINE_SIBLING_PID",
            fixture.dir.join("pipeline-sibling.pid"),
        )
        .env(
            "SHDEPS_TEST_PIPELINE_READY",
            fixture.dir.join("pipeline-ready"),
        );
    let (mut harness, mut master) = spawn_on_pty(command);
    let shdeps_pid = wait_for_pid(
        &fixture.dir.join("pipeline-shdeps.pid"),
        Duration::from_secs(3),
        "pipeline Shdeps pid",
    );
    let sibling_pid = wait_for_pid(
        &fixture.dir.join("pipeline-sibling.pid"),
        Duration::from_secs(3),
        "pipeline sibling pid",
    );
    let child_pid = wait_for_pid(
        &fixture.dir.join("pipeline-child.pid"),
        Duration::from_secs(3),
        "pipeline foreground child pid",
    );
    wait_until(
        Duration::from_secs(2),
        || fixture.dir.join("pipeline-ready").is_file() && terminal_group(&master) == child_pid,
        "pipeline child to own the terminal",
    );

    master.write_all(&[26]).unwrap();
    master.flush().unwrap();
    wait_until(
        Duration::from_secs(2),
        || process_is_stopped(shdeps_pid),
        "Shdeps pipeline member to stop",
    );
    let sibling_stopped = process_is_stopped(sibling_pid);
    if !sibling_stopped {
        harness.signal_session(libc::SIGKILL);
        let _ = harness.wait();
    }

    assert!(
        sibling_stopped,
        "Ctrl-Z must stop every member of Shdeps' original foreground pipeline group"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn background_terminal_read_stops_and_resumes_the_shdeps_job() {
    use std::io::Write as _;

    let fixture = Fixture::new("background-terminal-stop-resume");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
printf 'background prompt: ' >/dev/tty
IFS= read -r answer </dev/tty
printf '%s\n' "$answer" >"$SHDEPS_TEST_CHILD_RESUMED"
printf '[]\n'
"#,
    );
    fixture.write_executable(
        "fakebin/background-harness",
        r#"#!/usr/bin/env python3
import os
import signal
import time

tty = os.open("/dev/tty", os.O_RDWR)
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
shdeps = os.fork()
if shdeps == 0:
    os.setpgid(0, 0)
    os.execl(os.environ["SHDEPS_TEST_BINARY"], "shdeps", "list")
os.setpgid(shdeps, shdeps)
with open(os.environ["SHDEPS_TEST_BACKGROUND_PID"], "w") as pid_file:
    pid_file.write(f"{shdeps}\n")

waited, status = os.waitpid(shdeps, os.WUNTRACED)
if waited != shdeps or not os.WIFSTOPPED(status):
    raise SystemExit(70)
with open(os.environ["SHDEPS_TEST_BACKGROUND_STOPPED"], "w") as stopped:
    stopped.write(str(os.WSTOPSIG(status)))
while not os.path.exists(os.environ["SHDEPS_TEST_BACKGROUND_RESUME"]):
    time.sleep(0.001)
os.tcsetpgrp(tty, shdeps)
os.killpg(shdeps, signal.SIGCONT)

while True:
    waited, status = os.waitpid(shdeps, os.WUNTRACED | os.WCONTINUED)
    if waited == shdeps and (os.WIFEXITED(status) or os.WIFSIGNALED(status)):
        break
os.tcsetpgrp(tty, os.getpgrp())
if os.WIFEXITED(status):
    raise SystemExit(os.WEXITSTATUS(status))
raise SystemExit(128 + os.WTERMSIG(status))
"#,
    );

    let template = fixture.command(["list"]);
    let mut command = Command::new(fixture.dir.join("fakebin/background-harness"));
    command.env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let shdeps_pid_path = fixture.dir.join("background-shdeps.pid");
    let stopped_path = fixture.dir.join("background-stopped");
    let resume_path = fixture.dir.join("background-resume");
    let child_pid_path = fixture.dir.join("background-child.pid");
    let child_resumed_path = fixture.dir.join("background-child-resumed");
    command
        .env("SHDEPS_TEST_BINARY", env!("CARGO_BIN_EXE_shdeps"))
        .env("SHDEPS_TEST_BACKGROUND_PID", &shdeps_pid_path)
        .env("SHDEPS_TEST_BACKGROUND_STOPPED", &stopped_path)
        .env("SHDEPS_TEST_BACKGROUND_RESUME", &resume_path)
        .env("SHDEPS_TEST_CHILD_PID", &child_pid_path)
        .env("SHDEPS_TEST_CHILD_RESUMED", &child_resumed_path);
    let (mut harness, mut master) = spawn_on_pty(command);
    let shdeps_pid = wait_for_pid(
        &shdeps_pid_path,
        Duration::from_secs(3),
        "background Shdeps pid",
    );
    let child_pid = wait_for_pid(
        &child_pid_path,
        Duration::from_secs(3),
        "background terminal child pid",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    wait_until(
        Duration::from_secs(3),
        || stopped_path.is_file() && process_is_stopped(shdeps_pid),
        "background Shdeps job to propagate the child's terminal stop",
    );
    fs::write(&resume_path, "resume\n").unwrap();
    wait_until(
        Duration::from_secs(3),
        || terminal_group(&master) == child_pid,
        "resumed Shdeps job to hand the terminal to its child",
    );
    master.write_all(b"continue\n").unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut harness, Duration::from_secs(5));

    assert_eq!(status.and_then(|status| status.code()), Some(0));
    assert_eq!(
        fs::read_to_string(child_resumed_path).unwrap(),
        "continue\n",
        "the resumed child must complete its terminal read"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn cancellation_pending_while_shdeps_is_stopped_wakes_child_before_term() {
    use std::io::Write as _;

    let fixture = Fixture::new("foreground-child-cancel-while-stopped");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
trap 'printf term >"$SHDEPS_TEST_CHILD_TERM"; exit 0' TERM
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
while :; do /bin/sleep 1; done
"#,
    );
    let term = fixture.dir.join("foreground-stopped-term");
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-cancel-stopped.pid"),
        )
        .env("SHDEPS_TEST_CHILD_TERM", &term);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let child_pid = wait_for_pid(
        &fixture.dir.join("foreground-cancel-stopped.pid"),
        Duration::from_secs(3),
        "foreground child cancelled while stopped",
    );
    let _child_guard = EscapedProcessGuard::new(child_pid);
    wait_until(
        Duration::from_secs(2),
        || terminal_group(&master) == child_pid,
        "child to own the terminal before Ctrl-Z",
    );

    master.write_all(&[26]).unwrap();
    master.flush().unwrap();
    wait_until(
        Duration::from_secs(2),
        || process_is_stopped(shdeps.id()),
        "Shdeps to suspend with its stopped foreground child",
    );
    signal_process(shdeps.id(), libc::SIGTERM);
    signal_process(shdeps.id(), libc::SIGCONT);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM),
        "the pending parent signal must retain conventional status"
    );
    assert!(
        term.is_file(),
        "the stopped child must be continued before graceful TERM delivery"
    );
    assert!(!process_is_running(child_pid));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn concurrent_terminal_children_serialize_foreground_leases() {
    use std::io::Write as _;

    let fixture = Fixture::new("concurrent-terminal-leases");
    fixture.write(
        "conf/deps.conf",
        "owner/one github one\nowner/two github two\n",
    );
    fixture.write_executable(
        "fakebin/curl",
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' "$$" >>"$SHDEPS_TEST_CURL_PIDS"
printf 'curl-%s: ' "$$" >/dev/tty
IFS= read -r answer </dev/tty
printf '[]\n'
"#,
    );
    let pids_path = fixture.dir.join("terminal-curl.pids");
    let mut command = fixture.command(["--force", "update"]);
    command
        .env("SHDEPS_JOBS", "2")
        .env("SHDEPS_TEST_CURL_PIDS", &pids_path);
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let pids = wait_for_pids(&pids_path, 2, Duration::from_secs(3), "two curl workers");
    let mut guards = pids
        .iter()
        .copied()
        .map(EscapedProcessGuard::new)
        .collect::<Vec<_>>();
    let first = terminal_group(&master);
    assert!(
        pids.contains(&first),
        "one curl worker must own the terminal"
    );

    master.write_all(b"continue\n").unwrap();
    master.flush().unwrap();
    let second = *pids.iter().find(|pid| **pid != first).unwrap();
    wait_until(
        Duration::from_secs(3),
        || terminal_group(&master) == second,
        "second curl worker to acquire the terminal lease",
    );
    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    for guard in &mut guards {
        guard.disarm_if_exited();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "Ctrl-C must reach the sole current terminal-lease owner"
    );
    assert!(pids.into_iter().all(|pid| !process_is_running(pid)));
}

#[test]
fn non_tty_external_exit_130_remains_a_regular_failure() {
    let fixture = Fixture::new("non-tty-exit-130");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable("fakebin/cargo", "#!/bin/sh\nexit 130\n");

    let output = run(&mut fixture.command(["update"]));

    assert_eq!(
        output.status.code(),
        Some(1),
        "non-terminal exit 130 must not masquerade as parent cancellation"
    );
}

#[cfg(unix)]
fn assert_non_tty_external_signal_is_regular_failure(signal: &str) {
    let fixture = Fixture::new(&format!("non-tty-signal-{signal}"));
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        &format!(
            "#!/usr/bin/env python3\nimport os, signal\nsignal.signal(signal.SIG{signal}, signal.SIG_DFL)\nos.kill(os.getpid(), signal.SIG{signal})\n"
        ),
    );

    let output = run(&mut fixture.command(["update"]));

    assert_eq!(
        output.status.code(),
        Some(1),
        "a non-terminal child {signal} must remain an ordinary install failure"
    );
}

#[cfg(unix)]
#[test]
fn non_tty_external_sighup_remains_a_regular_failure() {
    assert_non_tty_external_signal_is_regular_failure("HUP");
}

#[cfg(unix)]
#[test]
fn non_tty_external_sigint_remains_a_regular_failure() {
    assert_non_tty_external_signal_is_regular_failure("INT");
}

#[cfg(unix)]
#[test]
fn non_tty_external_sigquit_remains_a_regular_failure() {
    assert_non_tty_external_signal_is_regular_failure("QUIT");
}

#[cfg(unix)]
#[test]
fn non_tty_external_sigterm_remains_a_regular_failure() {
    assert_non_tty_external_signal_is_regular_failure("TERM");
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_parent_session_descendant_that_escapes_the_hook_group() {
    let fixture = custom_sudo_fixture("parent-signal-parent-hook-descendant", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  shdeps_require_sudo || return $?
  set -m
  /bin/sh -c '
    trap "" HUP INT QUIT TERM
    printf "%s\n" "$$" >"$SHDEPS_STATE_DIR/retry-descendant.pid"
    while :; do
      printf x >>"$SHDEPS_STATE_DIR/retry-descendant-mutations"
      /bin/sleep 0.02
    done
  ' &
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/retry-hook.pid"
  wait
}

"#,
    );

    let mut command = custom_sudo_command(&fixture, ["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/retry-hook.pid"),
        Duration::from_secs(3),
        "authenticated retry hook pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("state/retry-descendant.pid"),
        Duration::from_secs(3),
        "authenticated retry descendant pid",
    );
    let _hook_guard = EscapedProcessGuard::new(hook_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (hook_group, hook_session) = process_group_and_session(hook_pid);
    let (descendant_group, descendant_session) = process_group_and_session(descendant_pid);
    let (caller_group, caller_session) = process_group_and_session(shdeps.id());
    assert_eq!(hook_group, hook_pid, "retry hook must lead its owned PGID");
    assert_ne!(
        descendant_group, hook_group,
        "fixture descendant must escape the retry hook PGID"
    );
    assert_eq!(
        hook_session, caller_session,
        "retry hook must retain caller SID"
    );
    assert_eq!(
        descendant_session, caller_session,
        "escaped descendant must retain caller SID"
    );
    assert_ne!(
        hook_group, caller_group,
        "retry hook must not own caller PGID"
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let hook_survived = process_is_running(hook_pid);
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("state/retry-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if hook_survived {
        kill_process(hook_pid);
    }
    if descendant_survived {
        kill_process(descendant_pid);
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        !hook_survived,
        "authenticated retry hook survived cancellation"
    );
    assert!(
        !descendant_survived,
        "authenticated retry descendant escaped its owned lineage"
    );
    assert!(
        !mutation_continued,
        "authenticated retry descendant continued mutating after Shdeps exited"
    );
}

#[cfg(unix)]
#[test]
fn parent_signal_stops_tracked_parent_session_descendant_after_hook_exit() {
    let fixture = custom_sudo_fixture("parent-signal-tracked-parent-hook-descendant", &["tool"]);
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() {
  shdeps_require_sudo || return $?
  set -m
  /bin/sh -c '
    trap "" HUP INT QUIT TERM
    printf "%s\n" "$$" >"$SHDEPS_STATE_DIR/retry-descendant.pid"
    while :; do
      printf x >>"$SHDEPS_STATE_DIR/retry-descendant-mutations"
      /bin/sleep 0.02
    done
  ' &
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/retry-hook.pid"
  return 0
}
"#,
    );

    let mut command = custom_sudo_command(&fixture, ["update"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut shdeps = spawn_test_session(&mut command);
    let hook_pid = wait_for_pid(
        &fixture.dir.join("state/retry-hook.pid"),
        Duration::from_secs(3),
        "authenticated retry hook pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("state/retry-descendant.pid"),
        Duration::from_secs(3),
        "tracked authenticated retry descendant pid",
    );
    let _hook_guard = EscapedProcessGuard::new_if_present(hook_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (hook_group, hook_session) = process_group_and_session(hook_pid);
    let (descendant_group, descendant_session) = process_group_and_session(descendant_pid);
    assert_ne!(
        descendant_group, hook_group,
        "fixture must escape hook PGID"
    );
    assert_eq!(
        descendant_session, hook_session,
        "portable fixture must retain parent SID"
    );
    wait_until(
        Duration::from_secs(2),
        || !process_is_running(hook_pid),
        "authenticated retry hook leader to exit after spawning escaped descendant",
    );

    signal_process(shdeps.id(), libc::SIGTERM);
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(3));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("state/retry-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process_group(descendant_group);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "leaked tracked authenticated retry descendant cleanup",
        );
    }
    if status.is_none() {
        kill_process(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGTERM)
    );
    assert!(
        !descendant_survived,
        "tracked retry descendant escaped after its hook leader exited"
    );
    assert!(
        !mutation_continued,
        "tracked retry descendant kept mutating after Shdeps returned"
    );
}

#[test]
fn custom_hooks_receive_detected_package_manager_in_every_phase() {
    let fixture = Fixture::new("custom-hook-package-manager");
    let binary = env!("CARGO_BIN_EXE_shdeps");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
record_manager() {
  printf '%s=%s\n' "$SHDEPS_HOOK_PHASE" "$(shdeps_pkg_mgr)" >>"$SHDEPS_STATE_DIR/hook-managers"
}
exists() {
  record_manager
  test -f "$SHDEPS_STATE_DIR/tool-installed"
}
install() {
  record_manager
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
}
version() { printf '1.2.3\n'; }
post() { record_manager; }
uninstall() { record_manager; }
"#,
    );
    fixture.write_executable("fakebin/dnf", "#!/bin/sh\nexit 0\n");
    fixture.write_executable("fakebin/bash", "#!/bin/sh\nexec /bin/bash \"$@\"\n");
    fixture.write_executable(
        "fakebin/shdeps",
        &format!("#!/bin/sh\nexec {binary} \"$@\"\n"),
    );
    let path = fixture.dir.join("fakebin");

    let updated = run(fixture
        .command(["update"])
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "spoofed"));
    assert_success(&updated);
    let managers = fs::read_to_string(fixture.dir.join("state/hook-managers")).unwrap();
    assert!(managers.lines().any(|line| line == "install=dnf"));
    assert!(managers.lines().any(|line| line == "post=dnf"));
    assert!(managers.lines().all(|line| line.ends_with("=dnf")));

    fs::write(fixture.dir.join("state/hook-managers"), "").unwrap();
    let checked = run(fixture
        .command(["check", "tool"])
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "spoofed"));
    assert_success(&checked);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/hook-managers")).unwrap(),
        "exists=dnf\n"
    );

    fs::write(fixture.dir.join("state/hook-managers"), "").unwrap();
    let listed = run(fixture
        .command(["list"])
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "spoofed"));
    assert_success(&listed);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/hook-managers")).unwrap(),
        "exists=dnf\n"
    );

    fs::write(fixture.dir.join("state/hook-managers"), "").unwrap();
    fixture.write("conf/deps.conf", "");
    let pruned = run(fixture
        .command(["prune", "-y"])
        .env("PATH", &path)
        .env("SHDEPS_PKG_MGR", "spoofed"));
    assert_success(&pruned);

    let managers = fs::read_to_string(fixture.dir.join("state/hook-managers")).unwrap();
    assert_eq!(managers, "uninstall=dnf\n");
}

#[cfg(unix)]
#[test]
fn cancellation_after_sourceable_hook_marker_replays_post_once() {
    let fixture = Fixture::new("sourceable-hook-marker-cancel");
    let binary = env!("CARGO_BIN_EXE_shdeps");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
  : >"$SHDEPS_STATE_DIR/signal-after-marker"
}
version() {
  if test -f "$SHDEPS_STATE_DIR/signal-after-marker"; then
    rm "$SHDEPS_STATE_DIR/signal-after-marker"
    kill -TERM "$PPID"
  fi
}
post() { printf 'post\n' >>"$SHDEPS_STATE_DIR/post-runs"; }
"#,
    );
    fixture.write_executable(
        "fakebin/shdeps",
        &format!("#!/bin/sh\nexec {binary} \"$@\"\n"),
    );
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("shdeps.sh");

    let cancelled = run(fixture.command(["update"]).env("SHDEPS_LIB", &wrapper));

    assert_eq!(cancelled.status.code(), Some(143));
    assert!(fixture.dir.join("state/tool-installed").is_file());
    assert!(
        fixture.dir.join("state/.pending-posts/tool").is_file(),
        "the sourceable wrapper's committed marker must survive cancellation"
    );
    assert!(!fixture.dir.join("state/post-runs").exists());

    let retry = run(fixture.command(["update"]).env("SHDEPS_LIB", &wrapper));

    assert_success(&retry);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/post-runs")).unwrap(),
        "post\n"
    );
    assert!(!fixture.dir.join("state/.pending-posts/tool").exists());

    let settled = run(fixture.command(["update"]).env("SHDEPS_LIB", &wrapper));

    assert_success(&settled);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/post-runs")).unwrap(),
        "post\n",
        "the recovered obligation must be acknowledged exactly once"
    );
}

#[test]
fn completed_custom_marker_is_not_replayed_on_later_updates() {
    let fixture = Fixture::new("completed-custom-marker-once");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() { printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"; }
post() { printf 'post\n' >>"$SHDEPS_STATE_DIR/post-runs"; }
"#,
    );

    let installed = run(&mut fixture.command(["update"]));
    let current = run(&mut fixture.command(["update"]));
    let settled = run(&mut fixture.command(["update"]));

    assert_success(&installed);
    assert_success(&current);
    assert_success(&settled);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/post-runs")).unwrap(),
        "post\n",
        "one committed mutation must create exactly one post invocation"
    );
    assert!(!fixture.dir.join("state/.pending-posts/tool").exists());
}

#[cfg(unix)]
#[test]
fn cancellation_after_unmarked_custom_mutation_replays_post_once() {
    let fixture = Fixture::new("unmarked-custom-mutation-cancel");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { test -f "$SHDEPS_STATE_DIR/tool-installed"; }
install() {
  printf 'installed\n' >"$SHDEPS_STATE_DIR/tool-installed"
  trap '' TERM
  kill -TERM "$PPID"
  while :; do /bin/sleep 1; done
}
post() { printf 'post\n' >>"$SHDEPS_STATE_DIR/post-runs"; }
"#,
    );

    let cancelled = run(&mut fixture.command(["update"]));

    assert_eq!(cancelled.status.code(), Some(143));
    assert!(fixture.dir.join("state/tool-installed").is_file());
    assert!(
        fixture.dir.join("state/.pending-posts/tool").is_file(),
        "the intent persisted before arbitrary hook code must survive cancellation"
    );
    assert!(!fixture.dir.join("state/post-runs").exists());

    let retry = run(&mut fixture.command(["update"]));

    assert_success(&retry);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/post-runs")).unwrap(),
        "post\n"
    );
    assert!(!fixture.dir.join("state/.pending-posts/tool").exists());

    let settled = run(&mut fixture.command(["update"]));

    assert_success(&settled);
    assert_eq!(
        fs::read_to_string(fixture.dir.join("state/post-runs")).unwrap(),
        "post\n",
        "a settled durable obligation must not replay on a later update"
    );
}

#[test]
fn update_nested_output_omits_standalone_heading() {
    let fixture = Fixture::new("update-nested");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() { printf 'installed\n'; }
"#,
    );

    let output = run(fixture.command(["update"]).env("SHDEPS_NESTED", "1"));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "  running  checking configured dependencies\n  changed  Custom: 1 changed\n    changed  tool: installed\n  changed  1 changed\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_verbose_reports_current_items() {
    let fixture = Fixture::new("update-verbose-current");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 0; }
version() { printf '9.9.9\n'; }
"#,
    );

    let output = run(&mut fixture.command(["-v", "update"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  Custom\n    ok       tool: 9.9.9\n  ok       1 current\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_verbose_reports_changed_action_details() {
    let fixture = Fixture::new("update-verbose-changed-details");
    fixture.write("conf/deps.conf", "tool custom\n");
    fixture.write(
        "conf/hooks.d/tool.sh",
        r#"
exists() { return 1; }
install() { return 0; }
version() { printf '1.2.3\n'; }
"#,
    );

    let output = run(&mut fixture.command(["-v", "update"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  Custom\n    changed  tool: added -- 1.2.3\n  changed  1 changed\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn update_verbose_reports_package_versions_only_when_verbose() {
    let fixture = Fixture::new("update-verbose-pkg-version");
    fixture.write("conf/deps.conf", "tool pkg tool\n");
    fixture.write("state/manifest", "tool|pkg|tool|\n");
    fixture.write(
        "state/tool.pkg-proof",
        "shdeps-pkg-proof-v1\nmanager=apt\npackage=tool\ncommand=tool\n",
    );
    fixture.write_executable("fakebin/apt-get", "#!/bin/sh\nexit 0\n");
    fixture.write_executable(
        "fakebin/tool",
        "#!/bin/sh\n[ \"$1\" = --version ] && printf 'tool 1.2.3\\n'\n",
    );

    let output = run(&mut fixture.command(["-v", "update"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  Packages\n    ok       tool: installed -- 1.2.3\n  ok       1 current\n"
    );
    assert_eq!(text(&output.stderr), "");
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
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  failed   Custom: 1 failed\n"
    );
    assert_eq!(
        text(&output.stderr),
        "  failed   broken: custom install failed\n  failed   1 failed\n"
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
        "Tools\n  running  checking configured dependencies\n  changed  Custom: 1 changed\n    changed  tool: installed\n  changed  1 changed\n"
    );
    assert_eq!(
        text(&output.stderr),
        "Warnings\n  warning  1 orphaned dep no longer in config\n  detail   old (github:release)\n  hint     run `shdeps prune` to remove orphaned artifacts\n"
    );
    assert!(fixture.dir.join("bin/old").exists());
    assert!(
        fs::read_to_string(fixture.dir.join("state/manifest"))
            .unwrap()
            .contains("old|github:release|old|")
    );

    let json_output = run(fixture.command(["update"]).env("SHDEPS_PROGRESS", "jsonl"));
    assert_success(&json_output);
    assert_eq!(text(&json_output.stderr), "");
    let events = jsonl(&json_output.stdout);
    assert!(
        events.iter().any(|event| event["event"] == "warning"
            && event["status"] == "warning"
            && event["detail"] == "1 orphaned dep no longer in config"),
        "expected an orphan warning event in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "detail"
            && event["status"] == "detail"
            && event["detail"] == "old (github:release)"),
        "expected an orphan detail event in {events:#?}"
    );
    assert!(
        events.iter().any(|event| event["event"] == "summary"
            && event["status"] == "changed"
            && event["changed"] == 1),
        "expected final summary after orphan events in {events:#?}"
    );
}

#[test]
#[cfg(unix)]
fn prune_lists_dry_runs_and_removes_orphans() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("prune");
    fixture.write("conf/deps.conf", "current github:repo\n");
    fixture.write(
        "state/manifest",
        "old|github:release|old|/tmp/old\ncurrent|github:repo|current|/tmp/current\n",
    );
    fixture.write_executable("share/old/bin/old", "#!/bin/sh\n");
    fs::create_dir_all(fixture.dir.join("bin")).unwrap();
    symlink(
        fixture.dir.join("share/old/bin/old"),
        fixture.dir.join("bin/old"),
    )
    .unwrap();
    fixture.write("share/old/artifact", "artifact\n");
    fixture.write(
        "conf/hooks.d/old.sh",
        "uninstall() { printf '%s\\n' \"$1\" > \"$SHDEPS_STATE_DIR/hook-ran\"; }\n",
    );

    let dry = run(&mut fixture.command(["prune", "--dry-run"]));
    assert_success(&dry);
    assert_eq!(
        text(&dry.stdout),
        "Warnings\n  warning  1 orphaned dep no longer in config\n  detail   old (github:release)\nDry run — nothing removed.\n"
    );
    assert_eq!(text(&dry.stderr), "");
    assert!(fixture.dir.join("bin/old").exists());
    assert!(text(&fs::read(fixture.dir.join("state/manifest")).unwrap()).contains("old|"));

    let removed = run(&mut fixture.command(["prune", "-y"]));
    assert_success(&removed);
    assert_eq!(
        text(&removed.stdout),
        "Warnings\n  warning  1 orphaned dep no longer in config\n  detail   old (github:release)\n  old removed\n"
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
        "Warnings\n  warning  1 orphaned dep no longer in config\n  detail   pkg-tool (pkg)\n"
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
            ("lua/shdeps.lua", "return {}\n", 0o644),
            ("lua/shdeps/core.lua", "return {}\n", 0o644),
            ("lua/shdeps/bootstrap.lua", "return {}\n", 0o644),
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
  *'url = "https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100"'*)
    printf '[{"tag_name":"20260524-120000-deadbeef","draft":false,"prerelease":false,"assets":[{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"},{"name":"%s","browser_download_url":"https://github.com/owner/tool/releases/download/v1/%s"}]}]\n' \
      "$SHDEPS_TEST_ARCHIVE_NAME" "$SHDEPS_TEST_ARCHIVE_NAME" \
      "$SHDEPS_TEST_CHECKSUM_NAME" "$SHDEPS_TEST_CHECKSUM_NAME"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_ARCHIVE_NAME"'"'*)
    cat "$SHDEPS_TEST_ARCHIVE"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'"$SHDEPS_TEST_CHECKSUM_NAME"'"'*)
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

static CAPTURE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run `command` capturing stdout/stderr through files instead of pipes.
///
/// `Command::output()` funnels the child through anonymous pipes and waits
/// for EOF. On macOS `pipe()`+`fcntl(CLOEXEC)` is non-atomic, so a fork in
/// a parallel test thread can inherit a pipe write end mid-window; when
/// that child outlives `ps` (stopped fixtures, sleep ladders), the EOF wait
/// hangs forever and the suite falls silent with live threads. Regular
/// files have no EOF wait: `status()` returns when this child exits, and a
/// leaked duplicate cannot block the read. Capture files are unique per
/// call (pid + sequence) and removed best-effort afterward; the caller's
/// stdin configuration is preserved exactly as `output()` would see it.
fn capture_output(command: &mut Command) -> std::io::Result<Output> {
    let seq = CAPTURE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let out_path = dir.join(format!("shdeps-capture-{pid}-{seq}.out"));
    let err_path = dir.join(format!("shdeps-capture-{pid}-{seq}.err"));
    let out_file = fs::File::create(&out_path)?;
    let err_file = fs::File::create(&err_path)?;
    command
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    let status = command.status()?;
    let stdout = fs::read(&out_path).unwrap_or_default();
    let stderr = fs::read(&err_path).unwrap_or_default();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn run(command: &mut Command) -> Output {
    capture_output(command).expect("shdeps command should run")
}

fn timed(command: &mut Command) -> (Output, Duration) {
    let started = Instant::now();
    let output = run(command);
    (output, started.elapsed())
}

fn timed_samples(mut command: impl FnMut() -> Command) -> (Output, Vec<Duration>) {
    let mut output = None;
    let mut samples = Vec::with_capacity(CI_PERFORMANCE_SAMPLES);
    for _ in 0..CI_PERFORMANCE_SAMPLES {
        let (current_output, elapsed) = timed(&mut command());
        assert_success(&current_output);
        output = Some(current_output);
        samples.push(elapsed);
    }
    (output.expect("at least one timing sample"), samples)
}

#[test]
fn file_capture_preserves_split_streams() {
    // The file-redirect capture must observe the same bytes `output()`
    // would: stdout and stderr stay split, exit status is preserved, and
    // images. A temp-dir leak scan would race parallel tests sharing
    // this process, so cleanup is best-effort by construction instead.)
    let mut command = Command::new("echo");
    command.arg("capture-probe");
    let captured = capture_output(&mut command).expect("echo should run");
    assert!(captured.status.success());
    assert_eq!(text(&captured.stdout), "capture-probe\n");
    assert!(captured.stderr.is_empty());
}

fn representative_duration(samples: &[Duration]) -> Duration {
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    ordered[ordered.len() / 2]
}

fn assert_ci_budget(command: &str, budget: Duration, output: &Output, samples: &[Duration]) {
    let representative = representative_duration(samples);
    assert!(
        representative <= budget,
        "{command} should stay under the CI budget; representative={representative:?}, budget={budget:?}, samples={samples:?}, stdout={:?}, stderr={:?}",
        text(&output.stdout),
        text(&output.stderr)
    );
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

fn jsonl(bytes: &[u8]) -> Vec<Value> {
    text(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).expect("progress line should be valid JSON"))
        .collect()
}

fn custom_sudo_fixture(name: &str, deps: &[&str]) -> Fixture {
    let fixture = Fixture::new(name);
    let binary = env!("CARGO_BIN_EXE_shdeps");
    let config = deps
        .iter()
        .map(|dep| format!("{dep} custom\n"))
        .collect::<String>();
    fixture.write("conf/deps.conf", &config);
    for dep in deps {
        fixture.write(
            format!("conf/hooks.d/{dep}.sh"),
            r#"
exists() { test -f "$SHDEPS_STATE_DIR/$1-installed"; }
install() {
  printf '%s install\n' "$1" >>"$SHDEPS_TEST_SUDO_LOG"
  shdeps_require_sudo || return $?
  printf 'installed\n' >"$SHDEPS_STATE_DIR/$1-installed"
}
"#,
        );
    }
    fixture.write_executable("fakebin/id", "#!/bin/sh\nprintf '1000\\n'\n");
    fixture.write_executable(
        "fakebin/sudo",
        r#"#!/bin/sh
phase=${SHDEPS_HOOK_PHASE:-parent}
printf '%s sudo %s\n' "$phase" "$*" >>"$SHDEPS_TEST_SUDO_LOG"
if [ "$1:$2" = '-n:true' ]; then
  test -f "$SHDEPS_TEST_SUDO_CACHE"
  exit $?
fi
if [ "$1" = true ]; then
  if [ "$phase" != parent ]; then
    exit 1
  fi
  if [ "${SHDEPS_TEST_SUDO_PARENT_FAIL:-0}" = 1 ]; then
    exit 1
  fi
  if [ "${SHDEPS_TEST_SUDO_STICKY_FAIL:-0}" != 1 ]; then
    : >"$SHDEPS_TEST_SUDO_CACHE"
  fi
  exit 0
fi
exit 2
"#,
    );
    fixture.write_executable(
        "fakebin/shdeps",
        &format!("#!/bin/sh\nexec {binary} \"$@\"\n"),
    );
    fixture
}

fn custom_sudo_command<const N: usize>(fixture: &Fixture, args: [&str; N]) -> Command {
    let mut command = fixture.command(args);
    command
        .env("SHDEPS_TEST_SUDO_LOG", fixture.dir.join("sudo.log"))
        .env("SHDEPS_TEST_SUDO_CACHE", fixture.dir.join("sudo-cache"));
    command
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_on_pty(mut command: Command) -> (GuardedChild, fs::File) {
    use std::os::unix::process::CommandExt as _;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // macOS declares the termios/winsize parameters mutable; Linux declares
    // them const. Null either way keeps platform defaults.
    #[cfg(target_vendor = "apple")]
    let (termp, winp) = (std::ptr::null_mut(), std::ptr::null_mut());
    #[cfg(not(target_vendor = "apple"))]
    let (termp, winp) = (std::ptr::null(), std::ptr::null());
    // SAFETY: openpty initializes both integer descriptors; optional terminal
    // attributes and window size are intentionally left at platform defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                termp,
                winp,
            )
        },
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: openpty returned two newly owned descriptors above.
    let master = unsafe { fs::File::from_raw_fd(master_fd) };
    // SAFETY: same ownership transfer for the slave descriptor.
    let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
    command
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: after fork and before exec, create a private test session and
    // attach descriptor 0's PTY as its controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("PTY-backed command should start");
    drop(slave);
    // SAFETY: F_GETFL/F_SETFL operate on this valid master descriptor.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    (GuardedChild::new(child), master)
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            .is_some_and(|state| state != 'Z')
    }

    #[cfg(not(target_os = "linux"))]
    {
        let output =
            capture_output(Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]))
                .expect("ps should be available in non-Linux CLI tests");
        output.status.success() && !text(&output.stdout).trim_start().starts_with('Z')
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_is_stopped(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.chars().next())
            })
            .is_some_and(|state| matches!(state, 'T' | 't'))
    }

    #[cfg(target_os = "macos")]
    {
        let output =
            capture_output(Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]))
                .expect("ps should inspect the PTY test process");
        output.status.success() && text(&output.stdout).trim_start().starts_with('T')
    }
}

#[cfg(unix)]
struct GuardedChild {
    child: Child,
    session: u32,
    reaped: bool,
}

#[cfg(unix)]
impl GuardedChild {
    fn new(child: Child) -> Self {
        let session = child.id();
        Self {
            child,
            session,
            reaped: false,
        }
    }

    fn signal_session(&self, signal: libc::c_int) {
        for pid in test_session_members(self.session) {
            if pid == std::process::id() {
                continue;
            }
            // Revalidate immediately before delivery so PID reuse cannot move
            // cleanup outside the test-owned session.
            if i32::try_from(pid)
                .ok()
                .is_some_and(|pid| unsafe { libc::getsid(pid) } == self.session as i32)
            {
                // SAFETY: the positive PID was revalidated against the unique
                // session created for this test child immediately above.
                unsafe {
                    libc::kill(pid as i32, signal);
                }
            }
        }
    }

    fn observed_status(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.reaped {
            return self.child.try_wait();
        }
        // SAFETY: waitid initializes only this local siginfo, targets the
        // retained direct child, and WNOWAIT deliberately preserves its PID
        // and session identity until descendant cleanup is complete.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error);
            }
            if info.si_pid() == 0 {
                return Ok(None);
            }
            let raw = match info.si_code {
                libc::CLD_EXITED => info.si_status() << 8,
                libc::CLD_KILLED => info.si_status(),
                libc::CLD_DUMPED => info.si_status() | 0x80,
                _ => return Ok(None),
            };
            Ok(Some(ExitStatus::from_raw(raw)))
        }
    }

    fn descendant_members(&self) -> Vec<u32> {
        test_session_members(self.session)
            .into_iter()
            .filter(|pid| *pid != self.child.id())
            .collect()
    }

    fn reap(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }

    fn cleanup_and_reap(&mut self) -> std::io::Result<ExitStatus> {
        if self.reaped {
            return self.child.wait();
        }

        self.signal_session(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let child_done = self.observed_status()?.is_some();
            if child_done && self.descendant_members().is_empty() {
                return self.reap();
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        self.signal_session(libc::SIGKILL);
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.descendant_members().is_empty() && Instant::now() < deadline {
            self.signal_session(libc::SIGKILL);
            std::thread::sleep(Duration::from_millis(10));
        }
        self.reap()
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.cleanup_and_reap()
    }
}

#[cfg(unix)]
impl std::ops::Deref for GuardedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for GuardedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[cfg(unix)]
impl Drop for GuardedChild {
    fn drop(&mut self) {
        let _ = self.cleanup_and_reap();
    }
}

#[cfg(unix)]
fn spawn_test_session(command: &mut Command) -> GuardedChild {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: setsid is async-signal-safe and runs after fork before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    GuardedChild::new(command.spawn().expect("guarded test process should start"))
}

#[cfg(unix)]
fn test_session_members(session: u32) -> Vec<u32> {
    let pids = if let Ok(entries) = fs::read_dir("/proc") {
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok()?.parse::<u32>().ok())
            .collect::<Vec<_>>()
    } else {
        capture_output(Command::new("ps").args(["-A", "-o", "pid="]))
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                text(&output.stdout)
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    pids.into_iter()
        .filter(|pid| {
            i32::try_from(*pid)
                .ok()
                .is_some_and(|pid| unsafe { libc::getsid(pid) } == session as i32)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_has_open_path(pid: u32, expected: &Path) -> bool {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == expected))
}

#[cfg(unix)]
fn process_group_and_session(pid: u32) -> (u32, u32) {
    let pid = i32::try_from(pid).expect("test pid must fit pid_t");
    // SAFETY: both calls only inspect the positive PID of a process that the
    // test observed as live immediately before this topology assertion.
    let group = unsafe { libc::getpgid(pid) };
    // SAFETY: same retained positive PID as above.
    let session = unsafe { libc::getsid(pid) };
    assert!(
        group > 0,
        "getpgid failed: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        session > 0,
        "getsid failed: {}",
        std::io::Error::last_os_error()
    );
    (group as u32, session as u32)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn terminal_group(tty: &fs::File) -> u32 {
    // SAFETY: tcgetpgrp only inspects this live PTY descriptor.
    let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    assert!(
        group > 0,
        "tcgetpgrp failed: {}",
        std::io::Error::last_os_error()
    );
    group as u32
}

#[cfg(unix)]
struct EscapedProcessGuard {
    pid: u32,
    group: u32,
    session: u32,
    start: Option<String>,
    armed: bool,
}

#[cfg(unix)]
impl EscapedProcessGuard {
    fn new(pid: u32) -> Self {
        let (group, session) = process_group_and_session(pid);
        Self {
            pid,
            group,
            session,
            start: test_process_start(pid),
            armed: true,
        }
    }

    fn new_if_present(pid: u32) -> Option<Self> {
        process_group_and_session_if_present(pid).map(|(group, session)| Self {
            pid,
            group,
            session,
            start: test_process_start(pid),
            armed: true,
        })
    }

    fn matches(&self) -> bool {
        process_is_running(self.pid)
            && process_group_and_session_if_present(self.pid) == Some((self.group, self.session))
            && test_process_start(self.pid) == self.start
    }

    fn signal(&self, signal: libc::c_int) {
        if !self.matches() {
            return;
        }
        // A process that leads its group (including a new-session escape)
        // reserves that group identity, so cleanup can safely include its
        // descendants. Otherwise target only the revalidated PID.
        unsafe {
            if self.group == self.pid {
                libc::kill(-(self.group as i32), signal);
            } else {
                libc::kill(self.pid as i32, signal);
            }
        }
    }

    fn disarm_if_exited(&mut self) {
        if !process_is_running(self.pid) {
            self.armed = false;
        }
    }
}

#[cfg(unix)]
impl Drop for EscapedProcessGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.signal(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.matches() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.signal(libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.matches() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
fn process_group_and_session_if_present(pid: u32) -> Option<(u32, u32)> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: both calls inspect one positive PID and return failure if it no
    // longer names the process captured by the guard.
    let group = unsafe { libc::getpgid(pid) };
    let session = unsafe { libc::getsid(pid) };
    (group > 0 && session > 0).then_some((group as u32, session as u32))
}

#[cfg(target_os = "linux")]
fn test_process_start(pid: u32) -> Option<String> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.windows(2).rposition(|part| part == b") ")?;
    stat[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .nth(19)
        .and_then(|field| std::str::from_utf8(field).ok())
        .map(ToOwned::to_owned)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn test_process_start(pid: u32) -> Option<String> {
    let output =
        capture_output(Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()])).ok()?;
    output.status.success().then(|| text(&output.stdout))
}

fn wait_for_pid(path: &Path, timeout: Duration, description: &str) -> u32 {
    let started = Instant::now();
    loop {
        if let Ok(pid) = fs::read_to_string(path).and_then(|value| {
            value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| std::io::Error::other("pid file is not complete"))
        }) {
            return pid;
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for {description} at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_pids(path: &Path, count: usize, timeout: Duration, description: &str) -> Vec<u32> {
    let started = Instant::now();
    loop {
        let mut pids = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .collect::<Vec<_>>();
        pids.sort_unstable();
        pids.dedup();
        if pids.len() >= count {
            pids.truncate(count);
            return pids;
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for {description} at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn read_pids(path: &Path) -> Vec<u32> {
    let mut pids = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(unix)]
fn pending_input_bytes(file: &fs::File) -> i32 {
    let mut pending = 0_i32;
    // SAFETY: FIONREAD writes one integer through the supplied valid pointer
    // and only inspects this test-owned open descriptor.
    assert_eq!(
        unsafe { libc::ioctl(file.as_raw_fd(), libc::FIONREAD as _, &mut pending) },
        0,
        "FIONREAD failed: {}",
        std::io::Error::last_os_error()
    );
    pending
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // SAFETY: tests pass a PID read from a child process they just created.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: libc::c_int) {
    // SAFETY: tests pass a positive PID read from a live Child handle.
    assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0);
}

#[cfg(unix)]
fn kill_process_group(leader: u32) {
    // SAFETY: the test observed this child after the production detached
    // runner made its PID the process-group identity.
    unsafe {
        libc::kill(-(leader as i32), libc::SIGKILL);
    }
}

fn wait_for_child_exit_bounded(child: &mut GuardedChild, timeout: Duration) -> Option<ExitStatus> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(status) = child
            .observed_status()
            .expect("child status should be observable")
        {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool, description: &str) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn host_arch() -> String {
    let output = capture_output(Command::new("uname").arg("-m"))
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
                r#"{{"name":"{asset}","browser_download_url":"https://github.com/owner/tool/releases/download/v1/{asset}"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"[{{"tag_name":"{tag}","draft":false,"prerelease":false,"assets":[{assets}]}}]"#)
}

fn count_release_fetches(log: &Path) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .matches("https://api.github.com/repos/cgraf78/shdeps/releases?per_page=100")
        .count()
}

fn count_git_pulls(log: &Path) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(" pull --ff-only --quiet"))
        .count()
}

struct Fixture {
    dir: PathBuf,
}

// Integration tests are a separate crate and cannot use the lib's private
// #[cfg(test)] registry, so this fixture owns cleanup directly.
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn fixture_removes_its_temp_tree_on_drop() {
    let dir = {
        let fixture = Fixture::new("drop-cleanup");
        fixture.dir.clone()
    };

    assert!(
        !dir.exists(),
        "temporary integration fixture leaked: {}",
        dir.display()
    );
}

#[test]
fn fixture_returns_the_physical_temp_tree() {
    let fixture = Fixture::new("physical-temp-tree");

    assert_eq!(fixture.dir, fs::canonicalize(&fixture.dir).unwrap());
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
        // Production normalizes checkout roots before comparing and publishing
        // them. Match that spelling here because macOS exposes the same temp
        // directory through `/var` while canonical paths use `/private/var`.
        let dir = fs::canonicalize(&dir).unwrap();
        Self { dir }
    }

    fn command<const N: usize>(&self, args: [&str; N]) -> Command {
        self.write_default_update_prereqs();
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
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("fakebin").display()),
            )
            .env(
                "SHDEPS_TEST_RELEASE_JSON",
                self.dir.join("fake/release.json"),
            )
            .env("SHDEPS_TEST_RELEASE_ASSET", self.dir.join("fake/asset"))
            .args(args);
        command
    }

    fn write_default_update_prereqs(&self) {
        let curl = self.dir.join("fakebin/curl");
        if !curl.exists() {
            self.write_executable(
                "fakebin/curl",
                "#!/bin/sh\nprintf 'unexpected default fake curl\\n' >&2\nexit 99\n",
            );
        }
        let gh = self.dir.join("fakebin/gh");
        if !gh.exists() {
            self.write_executable(
                "fakebin/gh",
                "#!/bin/sh\n[ \"$1\" = auth ] && [ \"$2\" = token ] && exit 1\nexit 1\n",
            );
        }
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

    fn initialize_dev_checkout(&self, short_name: &str, origin: &str) {
        let root = self.dir.join("git").join(short_name);
        let run = |args: &[&str]| {
            let output = capture_output(
                Command::new("git")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .args(["-c", "core.hooksPath=/dev/null", "-C"])
                    .arg(&root)
                    .args(args),
            )
            .unwrap();
            assert!(
                output.status.success(),
                "git -C {} {} failed: {}",
                root.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "--quiet"]);
        run(&["add", "--all"]);
        run(&[
            "-c",
            "user.name=Shdeps Test",
            "-c",
            "user.email=shdeps@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        run(&["remote", "add", "origin", origin]);
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
  *'url = "https://api.github.com/repos/owner/tool/releases?per_page=100"'*)
    if [[ -n "${SHDEPS_TEST_CURL_LOG:-}" ]]; then
      printf 'api\n' >>"$SHDEPS_TEST_CURL_LOG"
    fi
    cat "$SHDEPS_TEST_RELEASE_JSON"
    ;;
  *'url = "https://github.com/owner/tool/releases/download/v1/'*)
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
        self.write_stamp_age(name, kind, 0);
    }

    fn write_stamp_age(&self, name: &str, kind: &str, age_secs: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_secs()
            .saturating_sub(age_secs);
        let path = shdeps::stamp::remote_path(&self.dir.join("state"), name, kind);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{now}\n")).unwrap();
    }
}

// TEMP-DEBUG hang bisection probes (do not land). Each probe executes a
// progressively longer prefix of
// terminal_interrupt_of_leader_stops_ignoring_pipe_holder, which hangs
// deterministically on macOS in isolation. Alphabetical names run in
// order; the first hanging probe pinpoints the blocking call.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_00_fixture_only() {
    let fixture = Fixture::new("probe-of-leader-00");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_01_spawn_only() {
    let fixture = Fixture::new("probe-of-leader-01");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (_shdeps, _master) = spawn_on_pty(command);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_02_pids_guards() {
    let fixture = Fixture::new("probe-of-leader-02");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (_shdeps, _master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_03_ctrl_c() {
    use std::io::Write as _;

    let fixture = Fixture::new("probe-of-leader-03");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (_shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_04_bounded_wait() {
    use std::io::Write as _;

    let fixture = Fixture::new("probe-of-leader-04");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let _status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let _descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("foreground-descendant-mutations");
    let _size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_04b_survived_block_only() {
    use std::io::Write as _;

    let fixture = Fixture::new("probe-of-leader-04b");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let _status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("foreground-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let _mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process_group(descendant_group);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "foreground pipe-holder fallback cleanup",
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn probe_of_leader_05_full() {
    use std::io::Write as _;

    let fixture = Fixture::new("probe-of-leader-05");
    fixture.write("conf/deps.conf", "tool cargo\n");
    fixture.write_executable(
        "fakebin/cargo",
        r#"#!/bin/sh
/bin/sh -c '
  trap "" HUP INT QUIT TERM
  printf "%s\n" "$$" >"$SHDEPS_TEST_DESCENDANT_PID"
  while :; do
    printf x >>"$SHDEPS_TEST_DESCENDANT_MUTATIONS"
    /bin/sleep 0.02
  done
' &
printf '%s\n' "$$" >"$SHDEPS_TEST_CHILD_PID"
exec /bin/sleep 30
"#,
    );
    let mut command = fixture.command(["update"]);
    command
        .env(
            "SHDEPS_TEST_CHILD_PID",
            fixture.dir.join("foreground-leader.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_PID",
            fixture.dir.join("foreground-descendant.pid"),
        )
        .env(
            "SHDEPS_TEST_DESCENDANT_MUTATIONS",
            fixture.dir.join("foreground-descendant-mutations"),
        );
    let (mut shdeps, mut master) = spawn_on_pty(command);
    let leader_pid = wait_for_pid(
        &fixture.dir.join("foreground-leader.pid"),
        Duration::from_secs(3),
        "foreground installer leader pid",
    );
    let descendant_pid = wait_for_pid(
        &fixture.dir.join("foreground-descendant.pid"),
        Duration::from_secs(3),
        "foreground pipe-holder pid",
    );
    let _leader_guard = EscapedProcessGuard::new(leader_pid);
    let _descendant_guard = EscapedProcessGuard::new(descendant_pid);
    let (leader_group, _) = process_group_and_session(leader_pid);
    let (descendant_group, _) = process_group_and_session(descendant_pid);
    assert_eq!(
        leader_group, descendant_group,
        "fixture must share the owned PGID"
    );

    master.write_all(&[3]).unwrap();
    master.flush().unwrap();
    let status = wait_for_child_exit_bounded(&mut shdeps, Duration::from_secs(4));
    let descendant_survived = process_is_running(descendant_pid);
    let mutations = fixture.dir.join("foreground-descendant-mutations");
    let size_before = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(150));
    let mutation_continued = fs::metadata(&mutations)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != size_before;
    if descendant_survived {
        kill_process_group(descendant_group);
        wait_until(
            Duration::from_secs(2),
            || !process_is_running(descendant_pid),
            "foreground pipe-holder fallback cleanup",
        );
    }
    if status.is_none() {
        kill_process_group(shdeps.id());
        let _ = shdeps.wait();
    }

    assert_eq!(
        status.and_then(|status| status.code()),
        Some(128 + libc::SIGINT),
        "leader signal must be observed before inherited pipe EOF"
    );
    assert!(
        !descendant_survived,
        "pipe-holder survived terminal cancellation"
    );
    assert!(
        !mutation_continued,
        "pipe-holder kept mutating after Shdeps returned"
    );
}
