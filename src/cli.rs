//! CLI parsing and command dispatch for the Rust binary.
//!
//! This module owns user-facing command names, option parsing, exit codes, and
//! output text. Keeping formatting here prevents library modules from growing
//! incidental dependencies on terminal presentation.

use std::io::Write;

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
    let command_index = match parse_options(&args, stdout, stderr)? {
        ParseOutcome::Command(index) => index,
        ParseOutcome::Exit(code) => return Ok(code),
    };

    let command = args
        .get(command_index)
        .map(String::as_str)
        .unwrap_or("help");
    let rest = &args[(command_index + 1)..];

    match command {
        "version" => {
            writeln!(stdout, "{}", version::line())?;
            Ok(0)
        }
        "help" => {
            write!(stdout, "{HELP}")?;
            Ok(0)
        }
        "update" | "self-update" | "list" | "check" | "dep-root" | "dep-path" | "dep-file"
        | "prune" => not_implemented(command, rest, stderr),
        other => {
            writeln!(stderr, "error: unknown command '{other}'")?;
            writeln!(stderr, "Run 'shdeps help' for usage.")?;
            Ok(2)
        }
    }
}

enum ParseOutcome {
    Command(usize),
    Exit(i32),
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
                if args.get(index + 1).is_none() {
                    writeln!(stderr, "error: {arg} requires a path")?;
                    return Ok(ParseOutcome::Exit(2));
                }
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
            _ => return Ok(ParseOutcome::Command(index)),
        }
    }

    write!(stdout, "{HELP}")?;
    Ok(ParseOutcome::Exit(0))
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
