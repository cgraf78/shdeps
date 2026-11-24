//! Exercise the real version build script through Cargo without rebuilding dependencies.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_reftable(false).expect("initialize fixture")
    }

    /// Copy just the build script and its real inputs into an offline Cargo package.
    fn with_reftable(reftable: bool) -> Option<Self> {
        // Keep Cargo's sources away from parent checkout config and run its
        // output alongside this test, including on hosts with a noexec /tmp.
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("build-context.XXXXXX");
        let fixture = Self {
            root: PathBuf::from(run(Command::new("mktemp").arg("-d"))),
            target: PathBuf::from(run(Command::new("mktemp").arg("-d").arg(target))),
        };
        let repo = fixture.repo();
        Self::copy_package(&repo);
        let mut init = fixture.command("git", &repo);
        init.args(["init", "--quiet", "--initial-branch=main", "--template="]);
        if reftable {
            init.arg("--ref-format=reftable");
        }
        let output = init.output().unwrap();
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        if reftable && !output.status.success() && diagnostic.contains("unknown option") {
            return None;
        }
        assert!(output.status.success(), "init failed: {diagnostic}");
        fixture.git(&repo, &["config", "user.name", "Build Context Test"]);
        fixture.git(
            &repo,
            &["config", "user.email", "build-context@example.invalid"],
        );
        fixture.git(&repo, &["add", "."]);
        fixture.git(&repo, &["commit", "--quiet", "-m", "initial"]);
        if !reftable {
            // Keep both refs present so an unrelated missing path cannot force a rebuild.
            fixture.git(&repo, &["pack-refs", "--all", "--no-prune"]);
        }
        Some(fixture)
    }

    fn copy_package(repo: &Path) {
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir(repo.join("scripts")).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"build-context\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("src/main.rs"),
            r#"fn main() { println!("{}", env!("SHDEPS_BUILD_VERSION")); }"#,
        )
        .unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        for file in [
            "build.rs",
            "scripts/release.conf",
            "scripts/release-lib.sh",
            "scripts/release-version.sh",
        ] {
            fs::copy(source.join(file), repo.join(file)).unwrap();
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn linked(&self) -> PathBuf {
        let linked = self.root.join("linked");
        run(self
            .command("git", &self.repo())
            .args(["worktree", "add", "--quiet", "-b", "linked"])
            .arg(&linked));
        linked
    }

    /// Run only against this fixture's Git and Cargo configuration.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        run(self.command("git", repo).args(args))
    }

    // Use the toolchain that built this test, with its registry and targets in the fixture.
    fn cargo(&self, repo: &Path) -> Command {
        let cargo = Path::new(env!("CARGO"));
        let mut command = self.command(cargo, repo);
        command
            .args(["run", "--offline", "--verbose"])
            .env("RUSTC", cargo.with_file_name("rustc"))
            .env("CARGO_HOME", self.root.join("cargo-home"))
            .env(
                "CARGO_TARGET_DIR",
                self.target.join(repo.file_name().unwrap()),
            )
            .env("SHDEPS_BUILD_TIMESTAMP", "20010203-040506");
        command
    }

    fn version(&self, repo: &Path) -> String {
        format!(
            "20010203-040506-{}",
            &self.git(repo, &["rev-parse", "HEAD"])[..8]
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.target);
    }
}

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn run(command: &mut Command) -> String {
    String::from_utf8(checked(command).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn cargo_ignores_ambient_bash_startup_files() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let startup = fixture.root.join("bash-startup");
    fs::write(
        &startup,
        "touch \"$HOME/startup-ran\"; printf 'ambient startup\\n'\n",
    )
    .unwrap();

    assert_eq!(
        run(fixture
            .cargo(&repo)
            .env("BASH_ENV", &startup)
            .env("ENV", &startup)),
        fixture.version(&repo),
    );
    assert!(!fixture.root.join("startup-ran").exists());
}

#[test]
fn source_snapshot_never_adopts_an_unrelated_parent_commit() {
    let fixture = Fixture::new();
    let snapshot = fixture.repo().join("snapshot");
    Fixture::copy_package(&snapshot);

    let output = fixture.cargo(&snapshot).output().unwrap();
    assert!(
        !output.status.success(),
        "snapshot built without a source identity"
    );
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostics.contains("set SHDEPS_BUILD_COMMIT"),
        "{diagnostics}"
    );
}

#[test]
fn source_snapshot_accepts_an_explicit_commit() {
    let fixture = Fixture::new();
    let snapshot = fixture.repo().join("snapshot");
    Fixture::copy_package(&snapshot);

    assert_eq!(
        run(fixture
            .cargo(&snapshot)
            .env("SHDEPS_BUILD_COMMIT", "deadbeef")),
        "20010203-040506-deadbeef",
    );
}

#[test]
fn cargo_reuses_unchanged_linked_worktree_and_tracks_branch_commit() {
    let fixture = Fixture::new();
    let linked = fixture.linked();
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));

    let unchanged = checked(&mut fixture.cargo(&linked));
    let diagnostics = String::from_utf8(unchanged.stderr).unwrap();
    assert!(diagnostics.contains("Fresh build-context"), "{diagnostics}");

    fixture.git(
        &linked,
        &["commit", "--quiet", "--allow-empty", "-m", "new head"],
    );
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));
}

#[test]
fn cargo_refreshes_version_when_a_symbolic_branch_is_retargeted() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let next = fixture.git(
        &repo,
        &["commit-tree", "HEAD^{tree}", "-m", "alternate head"],
    );
    fixture.git(&repo, &["update-ref", "refs/heads/alternate", &next]);
    fixture.git(
        &repo,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
    );
    fixture.git(&repo, &["symbolic-ref", "HEAD", "refs/heads/alias"]);
    assert_eq!(run(&mut fixture.cargo(&repo)), fixture.version(&repo));

    // Only the intermediate ref changes. HEAD and both leaf refs stay put.
    fixture.git(
        &repo,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/alternate"],
    );
    assert_eq!(run(&mut fixture.cargo(&repo)), fixture.version(&repo));
}

#[test]
fn cargo_tracks_reftable_linked_branch_commit() {
    let Some(fixture) = Fixture::with_reftable(true) else {
        return; // Older Git builds have only the files backend.
    };
    let repo = fixture.repo();
    let next = fixture.git(&repo, &["commit-tree", "HEAD^{tree}", "-m", "new head"]);
    let linked = fixture.linked();
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));
    let unchanged = checked(&mut fixture.cargo(&linked));
    let diagnostics = String::from_utf8(unchanged.stderr).unwrap();
    assert!(diagnostics.contains("Fresh build-context"), "{diagnostics}");

    // Move the shared ref from the main checkout so the linked checkout's
    // private index cannot account for the Cargo rebuild.
    fixture.git(&repo, &["update-ref", "refs/heads/linked", &next]);
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));
}

#[test]
fn cargo_tracks_reftable_linked_detached_commit() {
    let Some(fixture) = Fixture::with_reftable(true) else {
        return; // Older Git builds have only the files backend.
    };
    let linked = fixture.linked();
    fixture.git(&linked, &["switch", "--quiet", "--detach"]);
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));

    fixture.git(
        &linked,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "new detached head",
        ],
    );
    assert_eq!(run(&mut fixture.cargo(&linked)), fixture.version(&linked));
}

#[test]
fn cargo_refreshes_version_after_release_configuration_changes() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    assert_eq!(run(&mut fixture.cargo(&repo)), fixture.version(&repo));

    let conf = repo.join("scripts/release.conf");
    let mut config = fs::read_to_string(&conf).unwrap();
    config.push_str("\nSHDEPS_BUILD_TIMESTAMP=19991231-235959\n");
    fs::write(conf, config).unwrap();
    let commit = fixture.git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        run(&mut fixture.cargo(&repo)),
        format!("19991231-235959-{}", &commit[..8]),
    );
}
