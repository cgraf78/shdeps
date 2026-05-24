//! CLI parsing and command dispatch for the Rust binary.
//!
//! This module owns user-facing command names, option parsing, exit codes, and
//! output text. Keeping formatting here prevents library modules from growing
//! incidental dependencies on terminal presentation.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::dep_path::{self, Roots};
use crate::errors::Error;
use crate::platform::{self, RuntimeEnv};
use crate::version;
use crate::Result;

/// Compatibility help text for the `shdeps` CLI.
///
/// Phase 1 keeps this in Rust even before every command is implemented because
/// help output is user-facing API. Later CLI parity work should update this
/// constant and the Bash reference together when command support changes.
pub const HELP: &str = "\
Usage: shdeps [options] <command> [args]

Commands:
  update          Install/update all dependencies
  self-update     Update shdeps itself (git pull, skips dirty trees)
  list            List all configured dependencies with status
  check <name>    Check if a specific dependency is installed
  dep-root <name> Print a configured dependency root directory
  dep-path <name> <rel>
                  Print a path below a configured dependency root
  dep-file <name> <rel>
                  Print a readable regular file below a dependency root
  prune           Remove orphaned deps no longer in config
  version         Print shdeps version
  help            Show this help message

Options:
  -c, --config <path>   Config directory or file (default: ~/.config/shdeps/)
  -f, --force           Bypass TTL cache (check for updates now)
  -R, --reinstall       Force reinstall all dependencies (implies --force)
  -q, --quiet           Suppress interactive prompts
  -v, --verbose         Verbose output (log level 2)

Prune options:
  -y                    Skip confirmation prompt
  --dry-run             Show what would be removed without removing

Examples:
  shdeps update
  shdeps -c ~/.config/myapp/ update
  shdeps --force update
  shdeps list
  shdeps check jq
  shdeps prune --dry-run
  shdeps prune -y

Exit codes:
  0  Success
  1  Error
  2  Usage error
";

/// Runs the Rust CLI and returns the process exit code.
///
/// The function takes explicit output streams so parity tests can capture exact
/// stdout/stderr text without spawning a subprocess for every CLI case. The
/// standalone binary is intentionally a thin adapter around this function.
pub fn run<I, S, W, E>(args: I, stdout: &mut W, stderr: &mut E) -> Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
    W: Write,
    E: Write,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let parsed = match parse_options(&args, stdout, stderr)? {
        ParseOutcome::Command(parsed) => parsed,
        ParseOutcome::Exit(code) => return Ok(code),
    };

    let command = args
        .get(parsed.command_index)
        .map(String::as_str)
        .unwrap_or("help");
    let rest = &args[(parsed.command_index + 1)..];

    match command {
        "version" => {
            writeln!(stdout, "{}", version::line())?;
            Ok(0)
        }
        "help" => {
            write!(stdout, "{HELP}")?;
            Ok(0)
        }
        "dep-root" => dep_root_cmd(rest, &parsed, stdout, stderr),
        "dep-path" => dep_path_cmd(rest, &parsed, stdout, stderr),
        "dep-file" => dep_file_cmd(rest, &parsed, stdout, stderr),
        "update" | "self-update" | "list" | "check" | "prune" => {
            not_implemented(command, rest, stderr)
        }
        other => {
            writeln!(stderr, "error: unknown command '{other}'")?;
            writeln!(stderr, "Run 'shdeps help' for usage.")?;
            Ok(2)
        }
    }
}

enum ParseOutcome {
    Command(ParsedOptions),
    Exit(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedOptions {
    command_index: usize,
    config_path: Option<PathBuf>,
}

fn parse_options<W, E>(args: &[String], stdout: &mut W, stderr: &mut E) -> Result<ParseOutcome>
where
    W: Write,
    E: Write,
{
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-c" | "--config" => {
                let Some(path) = args.get(index + 1) else {
                    writeln!(stderr, "error: --config requires an argument")?;
                    return Ok(ParseOutcome::Exit(2));
                };
                let _ = path;
                index += 2;
            }
            "-f" | "--force" | "-R" | "--reinstall" | "-q" | "--quiet" | "-v" | "--verbose"
            | "-y" | "--dry-run" => {
                index += 1;
            }
            "-h" | "--help" | "help" => {
                write!(stdout, "{HELP}")?;
                return Ok(ParseOutcome::Exit(0));
            }
            value if value.starts_with('-') => {
                writeln!(stderr, "error: unknown option '{value}'")?;
                writeln!(stderr, "Run 'shdeps help' for usage.")?;
                return Ok(ParseOutcome::Exit(2));
            }
            _ => {
                return Ok(ParseOutcome::Command(ParsedOptions {
                    command_index: index,
                    config_path: config_path(args),
                }));
            }
        }
    }

    write!(stdout, "{HELP}")?;
    Ok(ParseOutcome::Exit(0))
}

fn config_path(args: &[String]) -> Option<PathBuf> {
    let mut index = 0;
    let mut config = None;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-c" | "--config" => {
                if let Some(path) = args.get(index + 1) {
                    config = Some(PathBuf::from(path));
                }
                index += 2;
            }
            value if value.starts_with('-') => index += 1,
            _ => break,
        }
    }
    config
}

fn dep_root_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let Some(target) = args.first() else {
        writeln!(stderr, "error: dep-root requires a dependency name")?;
        writeln!(stderr, "Usage: shdeps dep-root <name>")?;
        return Ok(2);
    };

    run_path_lookup(
        dep_path::root(target, &roots(options), &runtime_env()),
        stdout,
    )
}

fn dep_path_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let (Some(target), Some(rel)) = (args.first(), args.get(1)) else {
        writeln!(
            stderr,
            "error: dep-path requires a dependency name and relative path"
        )?;
        writeln!(stderr, "Usage: shdeps dep-path <name> <relative-path>")?;
        return Ok(2);
    };

    run_path_lookup(
        dep_path::path(target, rel, &roots(options), &runtime_env()),
        stdout,
    )
}

fn dep_file_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let (Some(target), Some(rel)) = (args.first(), args.get(1)) else {
        writeln!(
            stderr,
            "error: dep-file requires a dependency name and relative path"
        )?;
        writeln!(stderr, "Usage: shdeps dep-file <name> <relative-path>")?;
        return Ok(2);
    };

    run_path_lookup(
        dep_path::file(target, rel, &roots(options), &runtime_env()),
        stdout,
    )
}

fn run_path_lookup<W>(result: Result<PathBuf>, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    match result {
        Ok(path) => {
            writeln!(stdout, "{}", path.display())?;
            Ok(0)
        }
        Err(Error::Resolve(error)) => Ok(error.exit_code()),
        Err(error) => Err(error),
    }
}

fn roots(options: &ParsedOptions) -> Roots {
    let home = home_dir();
    Roots {
        conf_dir: conf_dir(options, &home),
        git_dev_dir: env_path("SHDEPS_GIT_DEV_DIR").unwrap_or_else(|| home.join("git")),
        install_dir: env_path("SHDEPS_INSTALL_DIR").unwrap_or_else(|| home.join(".local/share")),
    }
}

fn conf_dir(options: &ParsedOptions, home: &Path) -> PathBuf {
    if let Some(path) = &options.config_path {
        return if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
    }

    env_path("SHDEPS_CONF_DIR").unwrap_or_else(|| {
        env_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("shdeps")
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("."))
}

fn runtime_env() -> RuntimeEnv {
    let platform = env::var("SHDEPS_TEST_PLATFORM").unwrap_or_else(|_| {
        let uname = command_output("uname", &["-s"]).unwrap_or_else(|| env::consts::OS.to_owned());
        let proc_version = fs::read_to_string("/proc/version").ok();
        platform::normalize_platform(uname.trim(), proc_version.as_deref())
    });
    let host = env::var("SHDEPS_TEST_HOST").unwrap_or_else(|_| {
        command_output("hostname", &["-s"])
            .or_else(|| command_output("hostname", &[]))
            .unwrap_or_else(|| "unknown".to_owned())
    });

    RuntimeEnv::new(platform, host.trim())
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn not_implemented<E>(command: &str, _rest: &[String], stderr: &mut E) -> Result<i32>
where
    E: Write,
{
    writeln!(
        stderr,
        "error: Rust command '{command}' is not implemented yet; use the Bash reference CLI"
    )?;
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::version;

    #[test]
    fn version_prints_embedded_commit() {
        let (code, stdout, stderr) = run_capture(["version"]);

        assert_eq!(code, 0);
        assert_eq!(stdout, format!("{}\n", version::line()));
        assert!(stderr.is_empty());
    }

    #[test]
    fn help_prints_usage() {
        let (code, stdout, stderr) = run_capture(["help"]);

        assert_eq!(code, 0);
        assert!(stdout.starts_with("Usage: shdeps"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn unknown_option_matches_reference_exit_code() {
        let (code, stdout, stderr) = run_capture(["--version"]);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            "error: unknown option '--version'\nRun 'shdeps help' for usage.\n"
        );
    }

    #[test]
    fn path_command_usage_errors_match_reference() {
        let (code, stdout, stderr) = run_capture(["dep-path", "cgraf78/sley"]);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            "error: dep-path requires a dependency name and relative path\nUsage: shdeps dep-path <name> <relative-path>\n"
        );
    }

    fn run_capture<const N: usize>(args: [&str; N]) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(args, &mut stdout, &mut stderr).expect("CLI run should not fail");

        (
            code,
            String::from_utf8(stdout).expect("stdout should be UTF-8"),
            String::from_utf8(stderr).expect("stderr should be UTF-8"),
        )
    }
}
