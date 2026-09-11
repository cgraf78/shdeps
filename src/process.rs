//! Host subprocess helpers used by install and status code.
//!
//! Process execution is deliberately isolated from higher-level dependency
//! logic. Shelling out is one of the easiest places to accidentally make warm
//! `shdeps` runs feel heavy, hang on an interactive tool, or diverge from the
//! Bash reference's `command -v` behavior. Keeping the rules here gives
//! `list`, `check`, package installs, and future cache probes the same answers.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cancellation::{self, Isolation};
use crate::tool_version;

pub(crate) const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const PACKAGE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL: Duration = Duration::from_millis(10);
const STOPPED_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
enum TimedIsolation {
    DetachedSession,
    ParentSession,
}

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

    /// Runs an absolute program from an absolute working directory with only
    /// the explicitly supplied environment and a mandatory timeout.
    ///
    /// This capability is intentionally fail-closed. Security-sensitive
    /// callers use it when inherited process state could select configuration,
    /// credentials, hooks, or helper programs. Ordinary fake runners must not
    /// silently degrade that boundary to `run`; they opt in only when their
    /// tests model the complete clean execution request.
    fn run_env_clear(
        &self,
        _program: &Path,
        _cwd: &Path,
        _args: &[OsString],
        _env: &BTreeMap<OsString, OsString>,
        _timeout: Duration,
    ) -> io::Result<Output> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runner does not support environment-cleared commands",
        ))
    }
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

    fn run_env_clear(
        &self,
        program: &Path,
        cwd: &Path,
        args: &[OsString],
        env: &BTreeMap<OsString, OsString>,
        timeout: Duration,
    ) -> io::Result<Output> {
        run_env_clear(program, cwd, args, env, timeout)
    }
}

/// Detects the active package manager using the Bash reference order.
#[must_use]
pub fn detect_package_manager(runner: &impl Runner) -> String {
    if runner.exists("brew")
        && runner
            // Bound `uname -s` to a short timeout. In container/VM
            // environments with broken syscall emulation, `uname` has
            // been observed to hang indefinitely; without a deadline,
            // the entire `shdeps list`/`update` warm path would block
            // on a probe that should answer in microseconds. The
            // SIGTERM-then-SIGKILL helper still gives the child a tiny
            // chance to exit cleanly.
            .run("uname", &["-s"], Some(VERSION_PROBE_TIMEOUT))
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
            if let Some(version) = record_version_probe(&mut probes, combined) {
                return Some(version);
            }
        }
    }

    for flag in ["--version", "-V"] {
        if let Ok(output) = runner.run(command, &[flag], Some(VERSION_PROBE_TIMEOUT)) {
            let combined = output.combined();
            if let Some(version) = record_version_probe(&mut probes, combined) {
                return Some(version);
            }
        }
    }

    let probe_refs = probes.iter().map(String::as_str).collect::<Vec<_>>();
    tool_version::extract(&probe_refs, command)
}

fn record_version_probe(probes: &mut Vec<String>, output: String) -> Option<String> {
    if tool_version::failed_to_load(&output) {
        return None;
    }

    if let Some(version) = tool_version::extract_dotted(&output) {
        return Some(version);
    }
    probes.push(output);
    None
}

/// Returns whether a package manager reports `package_name` as installed.
#[must_use]
pub fn package_installed(runner: &impl Runner, package_name: &str, pkg_mgr: &str) -> bool {
    if package_name.is_empty() {
        return false;
    }

    if pkg_mgr == "apt" {
        return runner
            .run(
                "dpkg-query",
                &["-W", "-f=${Status}\n", package_name],
                Some(PACKAGE_PROBE_TIMEOUT),
            )
            .ok()
            .is_some_and(|output| output.success && apt_status_installed(output.stdout.trim()));
    }

    let probe = match pkg_mgr {
        "brew" => Some(("brew", vec!["list", "--versions", package_name])),
        "dnf" | "zypper" => Some(("rpm", vec!["-q", package_name])),
        "pacman" => Some(("pacman", vec!["-Q", package_name])),
        "apk" => Some(("apk", vec!["info", "-e", package_name])),
        _ => None,
    };

    let Some((program, args)) = probe else {
        return false;
    };
    runner
        .run(program, &args, Some(PACKAGE_PROBE_TIMEOUT))
        .ok()
        .is_some_and(|output| output.success)
}

/// Loads installed package versions with one manager-specific batch query.
#[must_use]
pub fn package_versions(runner: &impl Runner, pkg_mgr: &str) -> BTreeMap<String, String> {
    let output = match pkg_mgr {
        "brew" => runner.run(
            "brew",
            &["list", "--formula", "--versions"],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "apt" => runner.run(
            "dpkg-query",
            &["-W", "-f=${Status}\t${Package}\t${Version}\n"],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "dnf" => runner.run(
            "rpm",
            &["-qa", "--qf", "%{NAME}\t%{VERSION}\n"],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "pacman" => runner.run("pacman", &["-Q"], Some(PACKAGE_PROBE_TIMEOUT)),
        // Bash loads package versions only for these managers today. Keeping
        // zypper/apk empty avoids pretending we have parity for output formats
        // the reference never parses.
        _ => return BTreeMap::new(),
    };

    let Ok(output) = output else {
        return BTreeMap::new();
    };
    if !output.success && (pkg_mgr != "brew" || output.stdout.trim().is_empty()) {
        return BTreeMap::new();
    }

    parse_package_versions(pkg_mgr, &output.stdout)
}

/// Returns one installed package's version without loading the manager's full inventory.
#[must_use]
pub fn package_version(runner: &impl Runner, package_name: &str, pkg_mgr: &str) -> Option<String> {
    if package_name.is_empty() {
        return None;
    }

    let output = match pkg_mgr {
        "brew" => runner.run(
            "brew",
            &["list", "--versions", package_name],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "apt" => runner.run(
            "dpkg-query",
            &["-W", "-f=${Status}\t${Package}\t${Version}\n", package_name],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "dnf" => runner.run(
            "rpm",
            &["-q", "--qf", "%{NAME}\t%{VERSION}\n", package_name],
            Some(PACKAGE_PROBE_TIMEOUT),
        ),
        "pacman" => runner.run("pacman", &["-Q", package_name], Some(PACKAGE_PROBE_TIMEOUT)),
        _ => return None,
    };

    let output = output.ok()?;
    if !output.success {
        return None;
    }

    parse_package_versions(pkg_mgr, &output.stdout)
        .into_values()
        .next()
}

/// Returns whether `path` is an executable regular file.
#[must_use]
pub fn executable_path(path: &Path) -> bool {
    is_executable(path)
}

fn run(program: &str, args: &[&str], timeout: Option<Duration>) -> io::Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    run_command(command, timeout)
}

/// Runs a bounded command in a new process group without leaving the caller's
/// session, so terminal-scoped credentials remain visible.
pub(crate) fn run_in_current_session(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    run_command_with_isolation(command, Some(timeout), TimedIsolation::ParentSession, true)
}

fn run_env_clear(
    program: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &BTreeMap<OsString, OsString>,
    timeout: Duration,
) -> io::Result<Output> {
    if !program.is_absolute() || !cwd.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "environment-cleared commands require absolute program and working-directory paths",
        ));
    }

    let mut command = Command::new(program);
    command.current_dir(cwd).args(args).env_clear().envs(env);
    run_command_with_isolation(
        command,
        Some(timeout),
        TimedIsolation::DetachedSession,
        true,
    )
}

fn run_command(command: Command, timeout: Option<Duration>) -> io::Result<Output> {
    run_command_with_isolation(command, timeout, TimedIsolation::DetachedSession, true)
}

fn run_command_with_isolation(
    mut command: Command,
    timeout: Option<Duration>,
    isolation: TimedIsolation,
    attribute_descendants: bool,
) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    cancellation::check()?;
    if timeout.is_none() {
        return if attribute_descendants {
            cancellation::output(command, None)
        } else {
            cancellation::output_without_attribution(command, None)
        }
        .map(convert_output);
    }
    let isolation = match timeout {
        Some(_) => match isolation {
            TimedIsolation::DetachedSession => Isolation::DetachedSession,
            TimedIsolation::ParentSession => Isolation::ParentSession,
        },
        // Unbounded commands historically inherited the foreground session and
        // process group. Keep that TTY/sudo behavior while retaining the exact
        // child PID for signal forwarding and reaping.
        None => Isolation::ExactChild,
    };
    let mut child = cancellation::spawn_owned(&mut command, isolation, attribute_descendants)?;
    // Drain both pipes while the child runs. Waiting for exit first can
    // deadlock once either pipe fills: the producer blocks on write while the
    // parent waits for a status that the blocked producer cannot reach.
    let stdout = child
        .take_stdout()
        .expect("piped child stdout must be available");
    let stderr = child
        .take_stderr()
        .expect("piped child stderr must be available");
    let (stdout_reader, stderr_reader) = cancellation::spawn_output_readers(
        &mut child,
        Box::new(move || read_pipe(stdout)),
        Box::new(move || read_pipe(stderr)),
    )?;
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut leader_exited = false;
    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        if cancellation::received_signal().is_some() {
            interrupted = true;
            break Some(child.stop(cancellation::TERMINATE_SIGNAL)?);
        }
        let output_drained = stdout_reader.is_finished() && stderr_reader.is_finished();
        if output_drained {
            if let Some(status) = child.wait_if_exited_and_output_drained()? {
                break Some(status);
            }
        } else if !leader_exited {
            leader_exited = child.exited()?;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Version probes are intentionally best-effort. Send SIGTERM
            // first so the complete owned boundary can run cleanup handlers,
            // then escalate to SIGKILL within the shared bounded grace.
            timed_out = true;
            break Some(child.stop(cancellation::TERMINATE_SIGNAL)?);
        }
        thread::sleep(WAIT_POLL);
    };
    let (stdout, stderr) = if interrupted || timed_out {
        let drain_deadline = Instant::now() + STOPPED_OUTPUT_DRAIN_GRACE;
        while !(stdout_reader.is_finished() && stderr_reader.is_finished())
            && Instant::now() < drain_deadline
        {
            thread::sleep(WAIT_POLL);
        }
        let stdout = if stdout_reader.is_finished() {
            join_pipe_reader(stdout_reader, "stdout")
        } else {
            cancellation::unfinished_output_reader("stdout")
        };
        let stderr = if stderr_reader.is_finished() {
            join_pipe_reader(stderr_reader, "stderr")
        } else {
            cancellation::unfinished_output_reader("stderr")
        };
        (stdout?, stderr?)
    } else {
        let stdout = join_pipe_reader(stdout_reader, "stdout");
        let stderr = join_pipe_reader(stderr_reader, "stderr");
        (stdout?, stderr?)
    };
    if interrupted || cancellation::received_signal().is_some() {
        return Err(io::Error::other("interrupted by signal"));
    }
    let status = status.expect("normal completion retains child status");
    Ok(Output {
        success: status.success(),
        timed_out,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn read_pipe(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stream: &str,
) -> io::Result<Vec<u8>> {
    cancellation::join_output_reader(reader, stream)
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
            "apt" => parse_apt_version_line(line),
            "dnf" => parse_tab_version_line(line),
            _ => None,
        };
        if let Some((name, version)) = parsed {
            versions.insert(name.to_owned(), version.to_owned());
        }
    }
    versions
}

fn parse_apt_version_line(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.splitn(3, '\t');
    let status = fields.next()?;
    let name = fields.next()?;
    let version = fields.next()?;
    (apt_status_installed(status) && !name.is_empty() && !version.is_empty())
        .then_some((name, version))
}

fn apt_status_installed(status: &str) -> bool {
    let mut fields = status.split_whitespace();
    fields.next().is_some()
        && fields.next() == Some("ok")
        && fields.next() == Some("installed")
        && fields.next().is_none()
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
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{
        Output, Process, Runner, command_path, dep_exists, dep_version, detect_package_manager,
        package_installed, package_version, package_versions,
    };

    #[derive(Debug, Default)]
    struct FakeRunner {
        commands: BTreeSet<String>,
        outputs: BTreeMap<(String, Vec<String>), Output>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
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

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
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
            self.calls.lock().unwrap().push((
                program.to_owned(),
                args.iter().copied().map(str::to_owned).collect(),
            ));
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
    fn environment_cleared_execution_fails_closed_for_runners_without_support() {
        let error = FakeRunner::default()
            .run_env_clear(
                Path::new("/absolute/program"),
                Path::new("/absolute/workdir"),
                &[],
                &BTreeMap::new(),
                Duration::from_secs(1),
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("environment-cleared"));
    }

    #[test]
    fn process_environment_cleared_execution_keeps_only_explicit_and_boundary_environment() {
        let program = command_path("env").expect("test host must provide env");
        let cwd = crate::test_support::temp_dir("shdeps-process-clean-env");
        let environment =
            BTreeMap::from([(OsString::from("SHDEPS_EXPLICIT"), OsString::from("present"))]);

        let output = Process
            .run_env_clear(&program, &cwd, &[], &environment, Duration::from_secs(2))
            .unwrap();

        assert!(output.success, "{}", output.stderr);
        let lines = output.stdout.lines().collect::<Vec<_>>();
        assert!(lines.contains(&"SHDEPS_EXPLICIT=present"));
        assert!(lines.iter().any(|line| {
            line.strip_prefix("SHDEPS_INTERNAL_PROCESS_BOUNDARIES=")
                .is_some_and(|value| !value.is_empty())
        }));
        assert_eq!(
            lines.len(),
            2,
            "clean execution may add only the private ownership marker"
        );
    }

    #[test]
    fn process_environment_cleared_execution_uses_requested_working_directory() {
        let program = command_path("pwd").expect("test host must provide pwd");
        // macOS commonly spells temporary paths through `/var`, while a
        // process with no inherited `PWD` reports the physical `/private/var`
        // path. Canonicalize the fixture so the assertion checks the requested
        // directory rather than an OS path alias.
        let cwd =
            fs::canonicalize(crate::test_support::temp_dir("shdeps-process-clean-cwd")).unwrap();

        let output = Process
            .run_env_clear(
                &program,
                &cwd,
                &[],
                &BTreeMap::new(),
                Duration::from_secs(2),
            )
            .unwrap();

        assert!(output.success, "{}", output.stderr);
        assert_eq!(Path::new(output.stdout.trim()), cwd);
    }

    #[test]
    fn process_environment_cleared_execution_drains_large_stdout_and_stderr() {
        let program = command_path("sh").expect("test host must provide sh");
        let cwd = crate::test_support::temp_dir("shdeps-process-large-output");
        let stdout_chunk = "a".repeat(1024);
        let stderr_chunk = "b".repeat(1024);
        let script = format!(
            "i=0; while [ \"$i\" -lt 2048 ]; do \
             printf '%s' '{stdout_chunk}'; \
             printf '%s' '{stderr_chunk}' >&2; \
             i=$((i + 1)); done"
        );

        let output = Process
            .run_env_clear(
                &program,
                &cwd,
                &[OsString::from("-c"), OsString::from(script)],
                &BTreeMap::new(),
                Duration::from_secs(10),
            )
            .unwrap();

        assert!(
            output.success,
            "large producer timed out: {}",
            output.stderr
        );
        assert!(!output.timed_out);
        assert_eq!(output.stdout.len(), 2 * 1024 * 1024);
        assert_eq!(output.stderr.len(), 2 * 1024 * 1024);
        assert!(output.stdout.bytes().all(|byte| byte == b'a'));
        assert!(output.stderr.bytes().all(|byte| byte == b'b'));
    }

    #[test]
    fn process_environment_cleared_execution_rejects_relative_paths() {
        let cwd = crate::test_support::temp_dir("shdeps-process-clean-relative");
        let environment = BTreeMap::<OsString, OsString>::new();

        let relative_program = Process
            .run_env_clear(
                Path::new("env"),
                &cwd,
                &[],
                &environment,
                Duration::from_secs(1),
            )
            .unwrap_err();
        let relative_cwd = Process
            .run_env_clear(
                Path::new("/absolute/program"),
                Path::new("relative/workdir"),
                &[],
                &environment,
                Duration::from_secs(1),
            )
            .unwrap_err();

        assert_eq!(relative_program.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(relative_cwd.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn detect_package_manager_bounds_uname_with_timeout() {
        // Container/VM environments with broken syscall emulation can
        // hang `uname` indefinitely. The detector must pass a finite
        // timeout to its only blocking probe so the warm path cannot
        // wedge on a misbehaving environment.
        use std::sync::Mutex;
        struct RecordingRunner {
            timeouts: Mutex<Vec<Option<Duration>>>,
        }
        impl Runner for RecordingRunner {
            fn exists(&self, command: &str) -> bool {
                command == "brew"
            }
            fn run(
                &self,
                _program: &str,
                _args: &[&str],
                timeout: Option<Duration>,
            ) -> io::Result<Output> {
                self.timeouts.lock().unwrap().push(timeout);
                Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: "Linux\n".to_owned(),
                    stderr: String::new(),
                })
            }
        }
        let runner = RecordingRunner {
            timeouts: Mutex::new(Vec::new()),
        };
        let _ = detect_package_manager(&runner);
        let observed = runner.timeouts.lock().unwrap().clone();
        assert!(
            observed
                .iter()
                .all(|timeout| timeout.is_some_and(|duration| duration > Duration::ZERO)),
            "uname probes must be passed a positive timeout: {observed:?}"
        );
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
            "dpkg-query",
            ["-W", "-f=${Status}\n", "bat"],
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
            .with_output(
                "dpkg-query",
                ["-W", "-f=${Status}\n", "font"],
                true,
                "install ok installed\n",
                "",
            )
            .with_output(
                "dpkg-query",
                ["-W", "-f=${Status}\n", "held"],
                true,
                "hold ok installed\n",
                "",
            )
            .with_output("rpm", ["-q", "font"], true, "font-1.0", "")
            .with_output("apk", ["info", "-e", "font"], true, "font", "");

        assert!(package_installed(&runner, "font", "apt"));
        assert!(package_installed(&runner, "held", "apt"));
        assert!(package_installed(&runner, "font", "zypper"));
        assert!(package_installed(&runner, "font", "apk"));
    }

    #[test]
    fn package_installed_rejects_apt_residual_config_state() {
        let runner = FakeRunner::default().with_output(
            "dpkg-query",
            ["-W", "-f=${Status}\n", "font"],
            true,
            "deinstall ok config-files\n",
            "",
        );

        assert!(!package_installed(&runner, "font", "apt"));
    }

    #[test]
    fn package_installed_uses_small_brew_version_probe() {
        let runner = FakeRunner::default().with_output(
            "brew",
            ["list", "--versions", "bash-completion@2"],
            true,
            "bash-completion@2 2.17.0\n",
            "",
        );

        assert!(package_installed(&runner, "bash-completion@2", "brew"));
    }

    #[test]
    fn package_installed_uses_bounded_probe_timeout() {
        let runner = TimeoutRecordingRunner::default();

        assert!(package_installed(&runner, "font", "apt"));

        assert_eq!(
            runner.timeouts(),
            vec![Some(super::PACKAGE_PROBE_TIMEOUT)],
            "package ownership probes must not run unbounded"
        );
    }

    #[test]
    fn package_versions_parse_manager_batch_outputs() {
        let runner = FakeRunner::default()
            .with_output(
                "dpkg-query",
                ["-W", "-f=${Status}\t${Package}\t${Version}\n"],
                true,
                "install ok installed\tbat\t1.2.3-1\n\
                 hold ok installed\theld\t4.0\n\
                 deinstall ok config-files\tremoved\t2.0\n\
                 install ok installed\tfd-find\t8.7.0\n",
                "",
            )
            .with_output(
                "brew",
                ["list", "--formula", "--versions"],
                true,
                "fzf 0.62.0 0.61.3\nripgrep 14.1.1\n",
                "",
            );

        let apt = package_versions(&runner, "apt");
        assert_eq!(apt.get("bat").map(String::as_str), Some("1.2.3-1"));
        assert_eq!(apt.get("fd-find").map(String::as_str), Some("8.7.0"));
        assert_eq!(apt.get("held").map(String::as_str), Some("4.0"));
        assert!(!apt.contains_key("removed"));

        let brew = package_versions(&runner, "brew");
        assert_eq!(brew.get("fzf").map(String::as_str), Some("0.62.0"));
        assert_eq!(brew.get("ripgrep").map(String::as_str), Some("14.1.1"));
    }

    #[test]
    fn package_version_uses_one_targeted_manager_query() {
        let runner = FakeRunner::default()
            .with_output(
                "dpkg-query",
                [
                    "-W",
                    "-f=${Status}\t${Package}\t${Version}\n",
                    "font-package",
                ],
                true,
                "install ok installed\tfont-package\t9.8.7\n",
                "",
            )
            .with_output(
                "rpm",
                ["-q", "--qf", "%{NAME}\t%{VERSION}\n", "font-package"],
                true,
                "font-package\t9.8.7\n",
                "",
            )
            .with_output(
                "pacman",
                ["-Q", "font-package"],
                true,
                "font-package 9.8.7\n",
                "",
            )
            .with_output(
                "brew",
                ["list", "--versions", "font-package"],
                true,
                "font-package 9.8.7\n",
                "",
            );

        for manager in ["apt", "dnf", "pacman", "brew"] {
            assert_eq!(
                package_version(&runner, "font-package", manager).as_deref(),
                Some("9.8.7")
            );
        }
        assert_eq!(runner.calls().len(), 4);
    }

    #[test]
    fn package_versions_keep_brew_stdout_when_snapshot_reports_cask_error() {
        let runner = FakeRunner::default().with_output(
            "brew",
            ["list", "--formula", "--versions"],
            false,
            "fzf 0.62.0\nripgrep 14.1.1\n",
            "Error: Refusing to load cask example from untrusted tap.\n",
        );

        let brew = package_versions(&runner, "brew");

        assert_eq!(brew.get("fzf").map(String::as_str), Some("0.62.0"));
        assert_eq!(brew.get("ripgrep").map(String::as_str), Some("14.1.1"));
    }

    #[test]
    fn package_versions_uses_bounded_probe_timeout() {
        let runner = TimeoutRecordingRunner::default();

        let versions = package_versions(&runner, "apt");

        assert_eq!(versions.get("tool").map(String::as_str), Some("1.0"));
        assert_eq!(
            runner.timeouts(),
            vec![Some(super::PACKAGE_PROBE_TIMEOUT)],
            "batch package snapshots must not run unbounded"
        );
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
    fn dep_version_stops_after_first_dotted_version() {
        let runner = FakeRunner::default()
            .with_output("tool", ["--version"], true, "tool 1.2.3\n", "")
            .with_output("tool", ["-V"], true, "tool 9.9.9\n", "");

        assert_eq!(dep_version(&runner, "tool").as_deref(), Some("1.2.3"));
        assert_eq!(
            runner.calls(),
            vec![("tool".to_owned(), vec!["--version".to_owned()])],
            "a definitive first probe must not launch the fallback process"
        );
    }

    #[test]
    fn dep_version_keeps_fallback_probe_for_integer_only_output() {
        let runner = FakeRunner::default()
            .with_output("tool", ["--version"], true, "tool version 12\n", "")
            .with_output("tool", ["-V"], true, "tool 1.2.3\n", "");

        assert_eq!(dep_version(&runner, "tool").as_deref(), Some("1.2.3"));
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn dep_version_does_not_mistake_fallback_punctuation_for_dotted_version() {
        let runner = FakeRunner::default()
            .with_output("tool", ["--version"], true, "tool version 12-beta.1\n", "")
            .with_output("tool", ["-V"], true, "tool 1.2.3\n", "");

        assert_eq!(dep_version(&runner, "tool").as_deref(), Some("1.2.3"));
        assert_eq!(runner.calls().len(), 2);
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

    #[cfg(unix)]
    #[test]
    fn timed_run_kills_grandchildren_that_keep_pipes_open() {
        let started = std::time::Instant::now();
        let output = super::run(
            "sh",
            &["-c", "sleep 30 & printf ready; wait"],
            Some(Duration::from_millis(100)),
        )
        .unwrap();

        assert!(output.timed_out);
        // Loaded macOS runners exceed 2s on snapshot-probed teardown while
        // still killing (not joining) the 30s grandchild; the ceiling stays
        // far below the join proof either way.
        let ceiling = if cfg!(target_os = "macos") {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        };
        assert!(
            started.elapsed() < ceiling,
            "timeout cleanup should not wait for a pipe-holding grandchild; elapsed={:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn current_session_timed_run_kills_pipe_holding_grandchild_after_leader_exits() {
        let started = std::time::Instant::now();
        let output = super::run_in_current_session(
            "sh",
            &["-c", "sleep 3 & printf ready; exit 1"],
            Duration::from_millis(100),
        )
        .unwrap();

        assert!(output.timed_out);
        assert!(!output.success);
        assert_eq!(output.stdout, "ready");
        // The ceiling must stay below the grandchild's 3s sleep to prove a
        // kill rather than a join; macOS gets the remaining headroom because
        // snapshot-probed teardown spikes past 2s under CI load.
        let ceiling = if cfg!(target_os = "macos") {
            Duration::from_millis(2500)
        } else {
            Duration::from_secs(2)
        };
        assert!(
            started.elapsed() < ceiling,
            "timeout cleanup should not join a pipe-holding grandchild; elapsed={:?}",
            started.elapsed()
        );
    }

    #[derive(Debug, Default)]
    struct TimeoutRecordingRunner {
        timeouts: std::sync::Mutex<Vec<Option<Duration>>>,
    }

    impl TimeoutRecordingRunner {
        fn timeouts(&self) -> Vec<Option<Duration>> {
            self.timeouts.lock().unwrap().clone()
        }
    }

    impl Runner for TimeoutRecordingRunner {
        fn exists(&self, _command: &str) -> bool {
            false
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            timeout: Option<Duration>,
        ) -> io::Result<Output> {
            self.timeouts.lock().unwrap().push(timeout);
            let stdout = if program == "dpkg-query" && args.get(1) == Some(&"-f=${Status}\n") {
                "install ok installed\n"
            } else if program == "dpkg-query" {
                "install ok installed\ttool\t1.0\n"
            } else {
                ""
            };
            Ok(Output {
                success: true,
                timed_out: false,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }
    }
}
