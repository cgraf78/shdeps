//! Host subprocess helpers used by install and status code.
//!
//! Process execution is deliberately isolated from higher-level dependency
//! logic. Shelling out is one of the easiest places to accidentally make warm
//! `shdeps` runs feel heavy, hang on an interactive tool, or diverge from the
//! Bash reference's `command -v` behavior. Keeping the rules here gives
//! `list`, `check`, package installs, and future cache probes the same answers.

use std::collections::BTreeMap;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::tool_version;

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Captured subprocess output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Whether the helper killed the process after the requested timeout.
    pub timed_out: bool,
    /// Captured stdout decoded lossily as UTF-8.
    pub stdout: String,
    /// Captured stderr decoded lossily as UTF-8.
    pub stderr: String,
}

impl Output {
    fn combined(&self) -> String {
        let mut output = self.stdout.clone();
        output.push_str(&self.stderr);
        output
    }
}

/// Subprocess abstraction for deterministic tests.
///
/// The production implementation uses the real host. Tests use a fake runner
/// so they do not mutate global `PATH`, depend on whatever package manager is
/// installed on the developer machine, or risk hanging on real commands.
pub trait Runner {
    /// Returns whether `command` is executable according to shell lookup rules.
    fn exists(&self, command: &str) -> bool;

    /// Returns the executable path used for shell lookup, when known.
    ///
    /// Most tests only care about present-vs-missing behavior, so the default
    /// implementation derives a stable synthetic path from `exists()`. The
    /// production runner overrides this with the real PATH result so cache keys
    /// notice command replacements even when the command name stays the same.
    fn path(&self, command: &str) -> Option<PathBuf> {
        self.exists(command).then(|| PathBuf::from(command))
    }

    /// Runs `program` with `args`, optionally enforcing `timeout`.
    fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> io::Result<Output>;
}

/// Real host subprocess runner.
#[derive(Debug, Clone, Copy, Default)]
pub struct Process;

impl Runner for Process {
    fn exists(&self, command: &str) -> bool {
        command_exists(command)
    }

    fn path(&self, command: &str) -> Option<PathBuf> {
        command_path(command)
    }

    fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>) -> io::Result<Output> {
        run(program, args, timeout)
    }
}

/// Detects the active package manager using the Bash reference order.
#[must_use]
pub fn detect_package_manager(runner: &impl Runner) -> String {
    if runner.exists("brew")
        && runner
            .run("uname", &["-s"], None)
            .ok()
            .is_some_and(|output| output.success && output.stdout.trim() == "Darwin")
    {
        return "brew".to_owned();
    }

    for (command, manager) in [
        ("apt-get", "apt"),
        ("dnf", "dnf"),
        ("pacman", "pacman"),
        ("zypper", "zypper"),
        ("apk", "apk"),
    ] {
        if runner.exists(command) {
            return manager.to_owned();
        }
    }

    String::new()
}

/// Returns whether a dependency is installed.
///
/// This mirrors `_shdeps_exists`: command lookup wins first because many
/// package names differ from their executable names. Package-manager ownership
/// is only consulted as a fallback so font packages and similar no-binary deps
/// can still report installed.
#[must_use]
pub fn dep_exists(runner: &impl Runner, command: &str, package_name: &str, pkg_mgr: &str) -> bool {
    dep_exists_with_versions(runner, command, package_name, pkg_mgr, &BTreeMap::new())
}

/// Returns whether a dependency is installed, using batch package data first.
///
/// `shdeps list` already pays for one manager-wide package-version snapshot on
/// platforms where the Bash reference knows how to parse it. Reusing that map
/// avoids a slow per-package `dpkg -s`/`rpm -q`/`pacman -Q` fallback whenever a
/// dependency has no command or the command name differs from the package name.
/// The final subprocess fallback is preserved for managers without batch data
/// and for stale snapshots.
#[must_use]
pub fn dep_exists_with_versions(
    runner: &impl Runner,
    command: &str,
    package_name: &str,
    pkg_mgr: &str,
    package_versions: &BTreeMap<String, String>,
) -> bool {
    if !command.is_empty() {
        if runner.exists(command) {
            return true;
        }

        // Git discovers subcommands through its exec path, so `git-foo` may be
        // valid even when it is not directly visible in PATH. The Bash helper
        // probes `git foo --version`; preserving that avoids false negatives
        // for git extension packages.
        if let Some(subcommand) = command.strip_prefix("git-") {
            if runner
                .run(
                    "git",
                    &[subcommand, "--version"],
                    Some(VERSION_PROBE_TIMEOUT),
                )
                .ok()
                .is_some_and(|output| output.success)
            {
                return true;
            }
        }
    }

    if package_versions.contains_key(package_name) {
        return true;
    }

    package_installed(runner, package_name, pkg_mgr)
}

/// Extracts an installed command version using Bash-compatible probes.
#[must_use]
pub fn dep_version(runner: &impl Runner, command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }

    let mut probes = Vec::new();
    if let Some(subcommand) = command
        .strip_prefix("git-")
        .filter(|_| !runner.exists(command))
    {
        if let Ok(output) = runner.run(
            "git",
            &[subcommand, "--version"],
            Some(VERSION_PROBE_TIMEOUT),
        ) {
            let combined = output.combined();
            if !tool_version::failed_to_load(&combined) {
                probes.push(combined);
            }
        }
    }

    for flag in ["--version", "-V"] {
        if let Ok(output) = runner.run(command, &[flag], Some(VERSION_PROBE_TIMEOUT)) {
            let combined = output.combined();
            if !tool_version::failed_to_load(&combined) {
                probes.push(combined);
            }
        }
    }

    let probe_refs = probes.iter().map(String::as_str).collect::<Vec<_>>();
    tool_version::extract(&probe_refs, command)
}

/// Returns whether a package manager reports `package_name` as installed.
#[must_use]
pub fn package_installed(runner: &impl Runner, package_name: &str, pkg_mgr: &str) -> bool {
    if package_name.is_empty() {
        return false;
    }

    let probe = match pkg_mgr {
        "brew" => Some(("brew", vec!["list", package_name])),
        "apt" => Some(("dpkg", vec!["-s", package_name])),
        "dnf" => Some(("rpm", vec!["-q", package_name])),
        "pacman" => Some(("pacman", vec!["-Q", package_name])),
        // Bash currently detects these managers for install selection but
        // does not use them in `_shdeps_exists`. Keep that asymmetry until the
        // reference behavior intentionally changes.
        _ => None,
    };

    let Some((program, args)) = probe else {
        return false;
    };
    runner
        .run(program, &args, None)
        .ok()
        .is_some_and(|output| output.success)
}

/// Loads installed package versions with one manager-specific batch query.
#[must_use]
pub fn package_versions(runner: &impl Runner, pkg_mgr: &str) -> BTreeMap<String, String> {
    let output = match pkg_mgr {
        "brew" => runner.run("brew", &["list", "--versions"], None),
        "apt" => runner.run("dpkg-query", &["-W", "-f=${Package}\t${Version}\n"], None),
        "dnf" => runner.run("rpm", &["-qa", "--qf", "%{NAME}\t%{VERSION}\n"], None),
        "pacman" => runner.run("pacman", &["-Q"], None),
        // Bash loads package versions only for these managers today. Keeping
        // zypper/apk empty avoids pretending we have parity for output formats
        // the reference never parses.
        _ => return BTreeMap::new(),
    };

    let Ok(output) = output else {
        return BTreeMap::new();
    };
    if !output.success {
        return BTreeMap::new();
    }

    parse_package_versions(pkg_mgr, &output.stdout)
}

/// Returns whether `path` is an executable regular file.
#[must_use]
pub fn executable_path(path: &Path) -> bool {
    is_executable(path)
}

fn run(program: &str, args: &[&str], timeout: Option<Duration>) -> io::Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let Some(timeout) = timeout else {
        return command.output().map(convert_output);
    };

    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(convert_output);
        }
        if Instant::now() >= deadline {
            // Version probes are intentionally best-effort. Killing the child
            // is preferable to letting a tool like an editor block `list` or
            // `check` indefinitely on a warm path.
            let _ = child.kill();
            let mut output = child.wait_with_output().map(convert_output)?;
            output.timed_out = true;
            return Ok(output);
        }
        thread::sleep(WAIT_POLL);
    }
}

fn convert_output(output: std::process::Output) -> Output {
    Output {
        success: output.status.success(),
        timed_out: false,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn parse_package_versions(pkg_mgr: &str, output: &str) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    for line in output.lines() {
        let parsed = match pkg_mgr {
            "brew" | "pacman" => parse_space_version_line(line),
            "apt" | "dnf" => parse_tab_version_line(line),
            _ => None,
        };
        if let Some((name, version)) = parsed {
            versions.insert(name.to_owned(), version.to_owned());
        }
    }
    versions
}

fn parse_space_version_line(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    let version = fields.next()?;
    Some((name, version))
}

fn parse_tab_version_line(line: &str) -> Option<(&str, &str)> {
    line.split_once('\t')
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())
}

fn command_exists(command: &str) -> bool {
    command_path(command).is_some()
}

fn command_path(command: &str) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    if command.contains('/') {
        return is_executable(Path::new(command)).then(|| PathBuf::from(command));
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "exe" | "cmd" | "bat"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::time::Duration;

    use super::{
        Output, Runner, dep_exists, dep_version, detect_package_manager, package_installed,
        package_versions,
    };

    #[derive(Debug, Default)]
    struct FakeRunner {
        commands: BTreeSet<String>,
        outputs: BTreeMap<(String, Vec<String>), Output>,
    }

    impl FakeRunner {
        fn with_command(mut self, command: &str) -> Self {
            self.commands.insert(command.to_owned());
            self
        }

        fn with_output<const N: usize>(
            mut self,
            program: &str,
            args: [&str; N],
            success: bool,
            stdout: &str,
            stderr: &str,
        ) -> Self {
            self.outputs.insert(
                (
                    program.to_owned(),
                    args.into_iter().map(str::to_owned).collect(),
                ),
                Output {
                    success,
                    timed_out: false,
                    stdout: stdout.to_owned(),
                    stderr: stderr.to_owned(),
                },
            );
            self
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, command: &str) -> bool {
            self.commands.contains(command)
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            self.outputs
                .get(&(
                    program.to_owned(),
                    args.iter().copied().map(str::to_owned).collect(),
                ))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
        }
    }

    #[test]
    fn detects_package_manager_in_bash_order() {
        let runner = FakeRunner::default()
            .with_command("brew")
            .with_command("apt-get")
            .with_output("uname", ["-s"], true, "Darwin\n", "");
        assert_eq!(detect_package_manager(&runner), "brew");

        let runner = FakeRunner::default()
            .with_command("brew")
            .with_command("apt-get")
            .with_output("uname", ["-s"], true, "Linux\n", "");
        assert_eq!(detect_package_manager(&runner), "apt");
    }

    #[test]
    fn dependency_exists_prefers_command_before_package_probe() {
        let runner = FakeRunner::default().with_command("bat").with_output(
            "dpkg",
            ["-s", "bat"],
            false,
            "",
            "missing",
        );

        assert!(dep_exists(&runner, "bat", "bat", "apt"));
    }

    #[test]
    fn dependency_exists_supports_git_subcommand_probe() {
        let runner = FakeRunner::default().with_output(
            "git",
            ["foo", "--version"],
            true,
            "git-foo 1.2.3",
            "",
        );

        assert!(dep_exists(&runner, "git-foo", "", ""));
    }

    #[test]
    fn package_installed_preserves_bash_manager_coverage() {
        let runner = FakeRunner::default()
            .with_output("dpkg", ["-s", "font"], true, "Package: font", "")
            .with_output("apk", ["info", "-e", "font"], true, "font", "");

        assert!(package_installed(&runner, "font", "apt"));
        assert!(!package_installed(&runner, "font", "apk"));
    }

    #[test]
    fn package_versions_parse_manager_batch_outputs() {
        let runner = FakeRunner::default()
            .with_output(
                "dpkg-query",
                ["-W", "-f=${Package}\t${Version}\n"],
                true,
                "bat\t1.2.3-1\nfd-find\t8.7.0\n",
                "",
            )
            .with_output(
                "brew",
                ["list", "--versions"],
                true,
                "fzf 0.62.0 0.61.3\nripgrep 14.1.1\n",
                "",
            );

        let apt = package_versions(&runner, "apt");
        assert_eq!(apt.get("bat").map(String::as_str), Some("1.2.3-1"));
        assert_eq!(apt.get("fd-find").map(String::as_str), Some("8.7.0"));

        let brew = package_versions(&runner, "brew");
        assert_eq!(brew.get("fzf").map(String::as_str), Some("0.62.0"));
        assert_eq!(brew.get("ripgrep").map(String::as_str), Some("14.1.1"));
    }

    #[test]
    fn package_versions_stay_empty_for_unparsed_managers() {
        let runner = FakeRunner::default().with_output(
            "apk",
            ["info", "-vv"],
            true,
            "tool-1.2.3 description\n",
            "",
        );

        assert!(package_versions(&runner, "apk").is_empty());
        assert!(package_versions(&runner, "").is_empty());
    }

    #[test]
    fn dep_version_merges_stderr_and_accepts_nonzero_output() {
        let runner = FakeRunner::default().with_output(
            "ssh",
            ["--version"],
            false,
            "",
            "OpenSSH_10.2p1, LibreSSL 3.3.6\n",
        );

        assert_eq!(dep_version(&runner, "ssh").as_deref(), Some("10.2p1"));
    }

    #[test]
    fn dep_version_skips_dynamic_loader_output() {
        let runner = FakeRunner::default().with_output(
            "bad",
            ["--version"],
            false,
            "",
            "bad: /lib64/libc.so.6: version `GLIBC_2.39' not found\n",
        );

        assert_eq!(dep_version(&runner, "bad"), None);
    }
}
