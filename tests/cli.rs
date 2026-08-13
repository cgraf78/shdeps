use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use shdeps::cli::{HELP, PUBLIC_COMMANDS};

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

    fixture.write_executable("share/cgraf78/sley/bin/sley", "#!/bin/sh\n");
    let (dep_links, links_elapsed) = timed(&mut fixture.command(["dep-links", "cgraf78/sley"]));
    assert_success(&dep_links);
    assert!(
        links_elapsed <= Duration::from_millis(200),
        "dep-links should stay under the CI cheap-command budget; elapsed={links_elapsed:?}, stdout={:?}, stderr={:?}",
        text(&dep_links.stdout),
        text(&dep_links.stderr)
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
fn update_jsonl_warns_when_local_clone_cannot_fast_forward() {
    let fixture = Fixture::new("update-jsonl-local-clone-diverged");
    fixture.write("conf/deps.conf", "owner/tool github:repo tool\n");
    fixture.write_executable("git/tool/bin/tool", "#!/bin/sh\n");
    fixture.write_executable(
        "fakebin/git",
        r##"#!/bin/sh
case " $* " in
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
    fixture.write(
        "conf/hooks.d/custom-tool.sh",
        r#"
exists() { return 0; }
version() { printf '9.9.9\n'; }
"#,
    );

    let output = run(&mut fixture.command(["-v", "update"]));

    assert_success(&output);
    assert_eq!(
        text(&output.stdout),
        "Tools\n  running  checking configured dependencies\n  GitHub\n    changed  owner/tool: added (local clone)\n  Custom\n    ok       custom-tool: 9.9.9\n  changed  1 changed, 1 current\n"
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
    // Catch an exit-polling regression that adds about 1.2 seconds across these
    // 30 serial current hooks without folding manifest I/O into the budget.
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
        "#!/bin/sh\nlast=\nfor arg do last=$arg; done\nprintf 'query %s\\n' \"$last\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$last\" = font-package ] || exit 1\nprintf 'font-package\\t9.8.7\\n'\n",
    );
    fixture.write_executable(
        "fakebin/dpkg",
        "#!/bin/sh\nprintf 'package %s\\n' \"$*\" >>\"$SHDEPS_TEST_LOG\"\n[ \"$*\" = '-s font-package' ]\n",
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
            .all(|line| line == "query fake-package"),
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
        "query font-package\n",
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
        "query missing-package\npackage -s missing-package\n"
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
fn update_bare_github_metadata_failure_rejects_repo_missing_explicit_command() {
    let fixture = Fixture::new("update-github-rate-limited-repo-missing-command");
    fixture.write("conf/deps.conf", "owner/tool github tool\n");
    fixture.write("git/tool/README.md", "source checkout without bin/tool\n");
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

fn jsonl(bytes: &[u8]) -> Vec<Value> {
    text(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).expect("progress line should be valid JSON"))
        .collect()
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
