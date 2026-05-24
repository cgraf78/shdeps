use std::fs;
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
}
