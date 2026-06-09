//! CLI parsing and command dispatch for the Rust binary.
//!
//! This module owns user-facing command names, option parsing, exit codes, and
//! output text. Keeping formatting here prevents library modules from growing
//! incidental dependencies on terminal presentation.

use std::collections::BTreeMap;
use std::env;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::Result;
use crate::api;
use crate::config::{self, Entry};
use crate::dep_links;
use crate::dep_path;
use crate::errors::Error;
use crate::github_method;
use crate::hooks::{BashCustomProbe, Uninstall};
use crate::http::Curl;
use crate::jobs;
use crate::manifest::{self, Manifest};
use crate::method;
use crate::process::{self, Process};
use crate::prune::{self, Options as PruneOptions};
use crate::runtime::{self, Overrides, ProcessEnv};
use crate::self_update::{self, Outcome, ReleaseArchiveOutcome, Target};
use crate::stamp;
use crate::status::{self, Context as StatusContext, DependencyStatus, SkipReason, State};
use crate::update::{self, Context as UpdateContext, Options as UpdateOptions};
use crate::version;
use serde_json::json;

/// Compatibility help text for the `shdeps` CLI.
///
/// Help output is user-facing API, so command support changes should update
/// this constant and its tests together.
pub const HELP: &str = "\
Usage: shdeps [options] <command> [args]

Commands:
  update                 Install/update all dependencies
  self-update            Update shdeps itself
  list                   List all configured dependencies with status
  check <name>           Check if a specific dependency is installed
  dep-root <name>        Print a configured dependency root directory
  dep-path <name> <rel>  Print a path below a configured dependency root
  dep-file <name> <rel>  Print a readable regular file below a dependency root
  dep-links <name>       Print public command links owned by a dependency
  prune                  Remove orphaned deps no longer in config
  version                Print shdeps version
  help                   Show this help message

Options:
  -c, --config <path>   Config directory or file (default: ~/.config/shdeps/)
  -f, --force           Bypass TTL cache (check for updates now)
  -R, --reinstall       Force reinstall all dependencies (implies --force)
  -q, --quiet           Suppress non-result output and interactive prompts
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
    run_inner(args, stdout, stderr, false)
}

/// Runs the CLI with optional live terminal progress for interactive callers.
pub fn run_terminal<I, S, W, E>(
    args: I,
    stdout: &mut W,
    stderr: &mut E,
    live_progress: bool,
) -> Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
    W: Write,
    E: Write,
{
    run_inner(args, stdout, stderr, live_progress)
}

fn run_inner<I, S, W, E>(
    args: I,
    stdout: &mut W,
    stderr: &mut E,
    live_progress: bool,
) -> Result<i32>
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
        "dep-links" => dep_links_cmd(rest, &parsed, stdout, stderr),
        "list" => list_cmd(rest, &parsed, stdout, stderr),
        "check" => check_cmd(rest, &parsed, stdout, stderr),
        "__api" => api::run(rest, &parsed.overrides, stdout, stderr),
        "migrate" => {
            writeln!(
                stderr,
                "error: migrate has been removed from the user-facing CLI"
            )?;
            writeln!(stderr, "Run 'shdeps help' for usage.")?;
            Ok(2)
        }
        "prune" => prune_cmd(rest, &parsed, stdout, stderr),
        "self-update" => self_update_cmd(rest, &parsed, stdout, stderr),
        "update" => update_cmd(rest, &parsed, stdout, stderr, live_progress),
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
    overrides: Overrides,
    quiet: bool,
    verbose: bool,
}

fn parse_options<W, E>(args: &[String], stdout: &mut W, stderr: &mut E) -> Result<ParseOutcome>
where
    W: Write,
    E: Write,
{
    let mut index = 0;
    let mut overrides = Overrides::default();
    // `SHDEPS_QUIET` is part of the sourceable API used by dotfiles and
    // hooks. Those callers often reach the Rust CLI through `shdeps_update`
    // rather than spelling `shdeps -q update`, so the environment must be
    // equivalent to the flag at the command boundary.
    let mut quiet = env::var("SHDEPS_QUIET").as_deref() == Ok("1");
    let mut verbose = env::var("SHDEPS_LOG_LEVEL").as_deref() == Ok("2");
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-c" | "--config" => {
                let Some(path) = args.get(index + 1) else {
                    writeln!(stderr, "error: --config requires an argument")?;
                    return Ok(ParseOutcome::Exit(2));
                };
                overrides.config = Some(PathBuf::from(path));
                index += 2;
            }
            "-f" | "--force" => {
                overrides.force = true;
                index += 1;
            }
            "-R" | "--reinstall" => {
                overrides.force = true;
                overrides.reinstall = true;
                index += 1;
            }
            "-q" | "--quiet" | "-v" | "--verbose" => {
                match arg.as_str() {
                    "-q" | "--quiet" => quiet = true,
                    "-v" | "--verbose" => verbose = true,
                    _ => unreachable!(),
                }
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
                    overrides,
                    quiet,
                    verbose,
                }));
            }
        }
    }

    write!(stdout, "{HELP}")?;
    Ok(ParseOutcome::Exit(0))
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

    dep_root(target, options, stdout)
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

    dep_path(target, rel, options, stdout)
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

    dep_file(target, rel, options, stdout)
}

fn dep_links_cmd<W, E>(
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
        writeln!(stderr, "error: dep-links requires a dependency name")?;
        writeln!(stderr, "Usage: shdeps dep-links <name>")?;
        return Ok(2);
    };

    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    dep_links_result(
        dep_links::links(target, &roots, &runtime::runtime_env(&ProcessEnv)),
        stdout,
    )
}

fn list_cmd<W, E>(
    _args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    _stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    let pkg_mgr = process::detect_package_manager(&Process);
    let raw_entries = config::load_dir(&roots.conf_dir)?;
    let entries = parse_entries(&raw_entries, &pkg_mgr);
    if entries.is_empty() {
        writeln!(stdout, "No dependencies configured.")?;
        return Ok(0);
    }

    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
    let manifest = manifest::read(&manifest::path(&roots.state_dir))?;
    let entries =
        resolve_github_entries(&entries, &roots, Some(&manifest), &env, &env_vars, options)?;
    let package_versions = if entries.iter().any(|entry| entry.method == method::PKG) {
        process::package_versions(&Process, &pkg_mgr)
    } else {
        BTreeMap::new()
    };
    let custom = custom_probe();
    let context = StatusContext {
        roots: &roots,
        env: &env,
        manifest: &manifest,
        runner: &Process,
        custom: &custom,
        pkg_mgr: &pkg_mgr,
        package_versions: &package_versions,
    };
    let statuses = status::list_with_jobs(&entries, &context, jobs::max(&env_vars))?;

    write_list(&statuses, stdout)?;
    Ok(0)
}

fn check_cmd<W, E>(
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
        writeln!(stderr, "error: check requires a dependency name")?;
        writeln!(stderr, "Usage: shdeps check <name>")?;
        return Ok(2);
    };

    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    let raw_entries = config::load_dir(&roots.conf_dir)?;
    let Some(raw_entry) = raw_entries.iter().find(|raw| {
        let entry = config::parse_entry(raw, None);
        entry.name == *target
    }) else {
        writeln!(stderr, "error: unknown dependency '{target}'")?;
        return Ok(1);
    };

    let probe_entry = config::parse_entry(raw_entry, None);
    let pkg_mgr = if probe_entry.method == method::PKG {
        process::detect_package_manager(&Process)
    } else {
        String::new()
    };
    let entry = config::parse_entry(
        raw_entry,
        if pkg_mgr.is_empty() {
            None
        } else {
            Some(&pkg_mgr)
        },
    );
    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
    let manifest = manifest::read(&manifest::path(&roots.state_dir))?;
    let entry = resolve_github_entry(&entry, &roots, Some(&manifest), &env, &env_vars, options)?;
    let package_versions = if entry.method == method::PKG {
        process::package_versions(&Process, &pkg_mgr)
    } else {
        BTreeMap::new()
    };
    let custom = custom_probe();
    let context = StatusContext {
        roots: &roots,
        env: &env,
        manifest: &manifest,
        runner: &Process,
        custom: &custom,
        pkg_mgr: &pkg_mgr,
        package_versions: &package_versions,
    };
    let dependency = status::classify(&entry, &context)?;

    write_check(&dependency, stdout)?;
    Ok(check_exit_code(&dependency.state))
}

fn update_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
    live_progress: bool,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let update_started = Instant::now();

    if let Some(arg) = args.first() {
        writeln!(stderr, "error: unknown update argument '{arg}'")?;
        return Ok(2);
    }
    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    ensure_bin_dir_on_path(&roots.bin_dir);

    let update_options = UpdateOptions {
        reinstall: runtime::reinstall(&ProcessEnv, &options.overrides),
        force: runtime::force(&ProcessEnv, &options.overrides),
        verbose: options.verbose,
        remote_ttl: remote_ttl(),
        ..UpdateOptions::default()
    };
    self_update_before_update(&roots, update_options, options, stderr)?;

    let pkg_mgr = process::detect_package_manager(&Process);
    let raw_entries = config::load_dir(&roots.conf_dir)?;
    let entries = parse_entries(&raw_entries, &pkg_mgr);
    if entries.is_empty() {
        if options.quiet {
            return Ok(0);
        }
        writeln!(stdout, "No dependencies configured.")?;
        return Ok(0);
    }

    let manifest_path = manifest::path(&roots.state_dir);
    let manifest = manifest::read(&manifest_path)?;
    let hooks = custom_probe();
    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
    if let Some(message) =
        update_prerequisite_error(&entries, &env, &Process, UpdatePrerequisitePhase::Initial)
    {
        writeln!(stderr, "{message}")?;
        return Ok(1);
    }
    let progress_jsonl = std::env::var_os("SHDEPS_PROGRESS").is_some_and(|value| value == "jsonl");
    let nested = std::env::var_os("SHDEPS_NESTED").is_some_and(|value| value == "1");
    let terminal_progress = live_progress && !options.quiet && !progress_jsonl;
    if terminal_progress && !nested {
        write_heading(stdout, "Tools")?;
    }

    let (entries, summary, active_count) = if terminal_progress {
        let mut progress = TtyProgress::new_colored(stdout, terminal_progress_plan(&entries, &env));
        progress.start()?;
        let entries = resolve_github_entries_with_progress_sink(
            &entries,
            &roots,
            Some(&manifest),
            &env,
            &env_vars,
            github_options_from_update(update_options),
            &mut progress,
        )?;
        if let Some(message) = update_prerequisite_error(
            &entries,
            &env,
            &Process,
            UpdatePrerequisitePhase::ConcreteOnly,
        ) {
            progress.clear_unfinished()?;
            writeln!(stderr, "{message}")?;
            return Ok(1);
        }
        let context = UpdateContext {
            manifest_path: &manifest_path,
            roots: &roots,
            env: &env,
            hooks: &hooks,
            runner: &Process,
            pkg_mgr: &pkg_mgr,
            env_vars: &env_vars,
            client: &Curl,
        };
        let active_count = entries
            .iter()
            .filter(|entry| update::active(entry, &env))
            .count();
        let summary = update::run_with_progress(
            &entries,
            &manifest,
            &context,
            update_options,
            &mut progress,
        )?;
        let footer = if nested {
            None
        } else {
            Some(format!(
                "Done in {}",
                elapsed_label_ms(update_started.elapsed().as_millis())
            ))
        };
        progress.finish(&summary, &entries, footer.as_deref())?;
        drop(progress);
        write_update_terminal_summary(
            &summary,
            &entries,
            options.quiet,
            options.verbose,
            stdout,
            stderr,
        )?;
        (entries, summary, active_count)
    } else {
        let entries = if progress_jsonl {
            resolve_github_entries_with_progress(
                &entries,
                &roots,
                Some(&manifest),
                &env,
                &env_vars,
                github_options_from_update(update_options),
                stdout,
            )?
        } else {
            resolve_github_entries_with_options(
                &entries,
                &roots,
                Some(&manifest),
                &env,
                &env_vars,
                github_options_from_update(update_options),
            )?
        };
        if let Some(message) = update_prerequisite_error(
            &entries,
            &env,
            &Process,
            UpdatePrerequisitePhase::ConcreteOnly,
        ) {
            writeln!(stderr, "{message}")?;
            return Ok(1);
        }
        let context = UpdateContext {
            manifest_path: &manifest_path,
            roots: &roots,
            env: &env,
            hooks: &hooks,
            runner: &Process,
            pkg_mgr: &pkg_mgr,
            env_vars: &env_vars,
            client: &Curl,
        };
        let active_count = entries
            .iter()
            .filter(|entry| update::active(entry, &env))
            .count();
        if !options.quiet && !progress_jsonl {
            if !nested {
                write_heading(stdout, "Tools")?;
            }
            write_row(stdout, "running", "checking configured dependencies")?;
        }

        let summary = if progress_jsonl {
            let mut progress = JsonlProgress { out: stdout };
            update::run_with_progress(&entries, &manifest, &context, update_options, &mut progress)?
        } else {
            let summary = update::run(&entries, &manifest, &context, update_options)?;
            write_update_summary(
                &summary,
                &entries,
                active_count,
                options.quiet,
                options.verbose,
                stdout,
                stderr,
            )?;
            summary
        };
        (entries, summary, active_count)
    };

    let manifest = manifest::read(&manifest_path)?;
    let orphans = manifest.orphans(&entries);
    if !orphans.is_empty() && !options.quiet {
        if progress_jsonl {
            let mut progress = JsonlProgress { out: stdout };
            write_prune_orphans_jsonl(&orphans, &mut progress)?;
        } else {
            write_prune_orphans(&orphans, true, stderr)?;
        }
    }
    if progress_jsonl {
        let mut progress = JsonlProgress { out: stdout };
        write_update_summary_jsonl(&summary, &entries, active_count, &mut progress)?;
    }

    Ok(if summary.has_errors() { 1 } else { 0 })
}

fn self_update_before_update<E>(
    roots: &runtime::Roots,
    update_options: UpdateOptions,
    options: &ParsedOptions,
    stderr: &mut E,
) -> Result<()>
where
    E: Write,
{
    let dir = match shdeps_install_dir() {
        Ok(dir) => dir,
        Err(error) => {
            if options.verbose {
                writeln!(
                    stderr,
                    "shdeps: update self-check could not resolve install dir ({error}); continuing"
                )?;
            }
            return Ok(());
        }
    };
    let target = match self_update::target(&dir) {
        Ok(target) => target,
        Err(error) => {
            if options.verbose {
                writeln!(
                    stderr,
                    "shdeps: update self-check could not inspect install ({error}); continuing"
                )?;
            }
            return Ok(());
        }
    };

    if matches!(target, Target::SourceCheckout) && env::var_os("SHDEPS_DIR").is_none() {
        return Ok(());
    }

    let stamp_path = stamp::remote_path(&roots.state_dir, "shdeps", "self-update");
    let freshness = stamp::Freshness {
        now: update_options.now,
        ttl: self_update_ttl(),
        force: update_options.force,
        reinstall: update_options.reinstall,
    };
    if stamp::remote_fresh(&stamp_path, freshness) {
        if options.verbose {
            writeln!(stderr, "shdeps: update self-check skipped; stamp is fresh")?;
        }
        return Ok(());
    }

    match target {
        Target::SourceCheckout => match self_update::source_checkout(&dir, &Process) {
            Ok(summary) => {
                if summary.outcome != Outcome::DirtySkipped {
                    touch_self_update_stamp(&stamp_path, update_options.now, options, stderr)?;
                }
                write_update_source_self_update_result(&summary, options.verbose, stderr)?;
            }
            Err(error) => {
                if options.verbose {
                    writeln!(
                        stderr,
                        "shdeps: update self-check failed ({error}); continuing"
                    )?;
                }
            }
        },
        Target::ReleaseArchive(metadata) => {
            // Stamp release attempts before network I/O. If GitHub is
            // unavailable or rate-limited, `shdeps update` should keep
            // managing dependencies without making every subsequent run
            // immediately retry the same failing endpoint.
            touch_self_update_stamp(&stamp_path, update_options.now, options, stderr)?;
            match self_update::release_archive(&dir, &metadata, &ProcessEnv, &Process, &Curl) {
                Ok(summary) => {
                    write_update_release_self_update_result(&summary, options.verbose, stderr)?;
                }
                Err(error) => {
                    if options.verbose {
                        writeln!(
                            stderr,
                            "shdeps: update self-check failed ({error}); continuing"
                        )?;
                    }
                }
            }
        }
        Target::Unsupported { .. } => {
            if options.verbose {
                writeln!(
                    stderr,
                    "shdeps: update self-check skipped; unsupported install"
                )?;
            }
        }
    }

    Ok(())
}

fn touch_self_update_stamp<E>(
    stamp_path: &Path,
    now: u64,
    options: &ParsedOptions,
    stderr: &mut E,
) -> Result<()>
where
    E: Write,
{
    if let Err(error) = stamp::remote_touch(stamp_path, now) {
        if options.verbose {
            writeln!(
                stderr,
                "shdeps: update self-check could not write stamp ({error})"
            )?;
        }
    }
    Ok(())
}

fn update_prerequisite_error(
    entries: &[Entry],
    env: &crate::platform::RuntimeEnv,
    runner: &impl process::Runner,
    phase: UpdatePrerequisitePhase,
) -> Option<String> {
    let missing = update_prerequisites(entries, env, phase)
        .into_iter()
        .filter(|prereq| !runner.exists(prereq.command))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return None;
    }

    let details = missing
        .iter()
        .map(|prereq| format!("{} ({})", prereq.command, prereq.reason))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "error: shdeps update is missing required tools for configured deps: {details}"
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdatePrerequisitePhase {
    /// Checks tools needed before bare `github` methods have concrete owners.
    Initial,
    /// Checks only concrete install methods after bare `github` resolution.
    ConcreteOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UpdatePrerequisite {
    command: &'static str,
    reason: &'static str,
}

fn update_prerequisites(
    entries: &[Entry],
    env: &crate::platform::RuntimeEnv,
    phase: UpdatePrerequisitePhase,
) -> Vec<UpdatePrerequisite> {
    let mut required = BTreeMap::<&'static str, UpdatePrerequisite>::new();
    for entry in entries {
        if !update::active(entry, env) {
            continue;
        }
        if let Some(prereq) =
            method::prerequisite(&entry.method, phase == UpdatePrerequisitePhase::Initial)
        {
            required
                .entry(prereq.command)
                .or_insert(UpdatePrerequisite {
                    command: prereq.command,
                    reason: prereq.reason,
                });
        }
    }
    required.into_values().collect()
}

struct JsonlProgress<'a, W>
where
    W: Write,
{
    out: &'a mut W,
}

impl<W> JsonlProgress<'_, W>
where
    W: Write,
{
    fn event(&mut self, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.out, &value)?;
        writeln!(self.out)?;
        Ok(())
    }
}

impl<W> update::Progress for JsonlProgress<'_, W>
where
    W: Write,
{
    fn phase(&mut self, phase: update::Phase<'_>) -> Result<()> {
        self.event(json!({
            "event": "phase",
            "group": phase.group,
            "phase": phase.key,
            "label": phase.label,
            "status": phase.status,
            "detail": phase.detail,
            "done": phase.done,
            "total": phase.total,
        }))
    }

    fn item(&mut self, group: &'static str, item: &update::Item) -> Result<()> {
        self.event(json!({
            "event": "item",
            "group": group,
            "status": item_status(item),
            "name": item.name.as_str(),
            "detail": item.detail.as_str(),
        }))
    }
}

struct TtyProgress<'a, W>
where
    W: Write,
{
    out: &'a mut W,
    rows: Vec<TtyProgressRow>,
    active: bool,
    finished: bool,
    rendered_rows: usize,
    color: bool,
}

struct TtyProgressRow {
    stage: &'static str,
    label: String,
    started: Option<Instant>,
    elapsed_ms: Option<u128>,
    done: usize,
    total: usize,
    touched: bool,
    components: BTreeMap<&'static str, (usize, usize)>,
}

impl<W> TtyProgress<'_, W>
where
    W: Write,
{
    #[cfg(test)]
    fn new(out: &mut W, stages: Vec<TtyProgressStage>) -> TtyProgress<'_, W> {
        TtyProgress::new_with_color(out, stages, false)
    }

    fn new_colored(out: &mut W, stages: Vec<TtyProgressStage>) -> TtyProgress<'_, W> {
        TtyProgress::new_with_color(out, stages, color_enabled())
    }

    fn new_with_color(
        out: &mut W,
        stages: Vec<TtyProgressStage>,
        color: bool,
    ) -> TtyProgress<'_, W> {
        TtyProgress {
            out,
            rows: stages
                .into_iter()
                .map(|stage| TtyProgressRow {
                    stage: stage.stage,
                    label: progress_label(update::label_for_display_group(stage.stage)),
                    started: None,
                    elapsed_ms: None,
                    done: 0,
                    total: stage.total,
                    touched: false,
                    components: BTreeMap::new(),
                })
                .collect(),
            active: false,
            finished: false,
            rendered_rows: 0,
            color,
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let rows = self.live_rows();
        self.rewrite_rows(&rows, false, None)
    }

    fn finish(
        &mut self,
        summary: &update::Summary,
        entries: &[Entry],
        footer: Option<&str>,
    ) -> Result<()> {
        if self.rows.is_empty() {
            self.finished = true;
            self.active = false;
            self.rendered_rows = 0;
            return Ok(());
        }

        let final_rows = self.final_rows(summary, entries);
        self.rewrite_rows(&final_rows, true, footer)?;
        self.finished = true;
        self.active = false;
        self.rendered_rows = 0;
        Ok(())
    }

    fn final_rows(&self, summary: &update::Summary, entries: &[Entry]) -> Vec<RenderedTtyRow> {
        let mut summaries = BTreeMap::new();
        for group in update::dashboard_group_order().iter().copied() {
            let items = summary
                .items
                .iter()
                .filter(|item| dashboard_group_for_name(entries, &item.name) == group)
                .collect::<Vec<_>>();
            if items.is_empty() {
                continue;
            }
            let counts = update_counts_for_items(&items);
            let elapsed_ms = dashboard_elapsed_ms(summary, group);
            summaries.insert(
                group,
                RenderedTtyRow {
                    status: update_status(counts).to_owned(),
                    detail: format!(
                        "{}: {}",
                        update::label_for_display_group(group),
                        update_count_summary(counts)
                    ),
                    elapsed: elapsed_label_ms(elapsed_ms),
                },
            );
        }

        let mut rows = Vec::new();
        for row in self.ordered_rows() {
            if let Some(mut summary) = summaries.remove(row.stage) {
                if row.touched {
                    summary.elapsed = elapsed_label_ms(row.elapsed_ms.unwrap_or_else(|| {
                        row.started
                            .map_or(0, |started| started.elapsed().as_millis())
                    }));
                }
                rows.push(summary);
            } else if row.touched {
                rows.push(RenderedTtyRow {
                    status: "ok".to_owned(),
                    detail: progress_done_detail(row.stage, &row.label, row.total),
                    elapsed: elapsed_label_ms(row.elapsed_ms.unwrap_or_else(|| {
                        row.started
                            .map_or(0, |started| started.elapsed().as_millis())
                    })),
                });
            }
        }
        rows
    }

    fn rewrite_rows(
        &mut self,
        rows: &[RenderedTtyRow],
        finish_line: bool,
        footer: Option<&str>,
    ) -> Result<()> {
        let previous_rows = self.rendered_rows;
        if self.active {
            write!(self.out, "\r\x1b[K")?;
            if previous_rows > 1 {
                write!(self.out, "\x1b[{}A", previous_rows - 1)?;
            }
        }

        let rendered_rows = rows.len() + usize::from(footer.is_some());
        let line_count = rendered_rows.max(previous_rows);
        for index in 0..line_count {
            write!(self.out, "\r\x1b[K")?;
            if let Some(row) = rows.get(index) {
                write!(
                    self.out,
                    "{}",
                    tty_row_with_color(&row.status, &row.detail, &row.elapsed, self.color)
                )?;
            } else if index == rows.len()
                && let Some(footer) = footer
            {
                write!(self.out, "{footer}")?;
            }
            if index + 1 < line_count {
                writeln!(self.out)?;
            }
        }

        if finish_line {
            let cleared_rows = line_count.saturating_sub(rendered_rows);
            if rows.is_empty() {
                writeln!(self.out)?;
            } else {
                if cleared_rows > 0 {
                    write!(self.out, "\x1b[{}A", cleared_rows)?;
                }
                writeln!(self.out)?;
            }
        } else if line_count > rendered_rows && !rows.is_empty() {
            write!(self.out, "\x1b[{}A", line_count - rendered_rows)?;
        }
        self.out.flush()?;
        self.active = !finish_line;
        self.rendered_rows = if finish_line { 0 } else { rendered_rows };
        Ok(())
    }

    fn progress_row(&mut self, label: &str, phase: &str, done: usize, total: usize) -> Result<()> {
        let spec = update::phase_spec(phase);
        let stage = spec.dashboard_stage;
        let component = spec.component;
        let label = progress_label(label);
        let index = match self.rows.iter().position(|row| row.stage == stage) {
            Some(index) => index,
            None => {
                self.rows.push(TtyProgressRow {
                    stage,
                    label,
                    started: None,
                    elapsed_ms: None,
                    done,
                    total,
                    touched: false,
                    components: BTreeMap::new(),
                });
                self.rows.len() - 1
            }
        };

        {
            let row = &mut self.rows[index];
            let started = *row.started.get_or_insert_with(Instant::now);
            row.components.insert(component, (done, total));
            row.total = row.total.max(total);
            row.done = progress_done(row);
            row.touched = true;
            row.elapsed_ms = Some(started.elapsed().as_millis());
        }

        let rows = self.live_rows();
        self.rewrite_rows(&rows, false, None)
    }

    fn clear_unfinished(&mut self) -> Result<()> {
        if !self.active || self.finished || self.rows.is_empty() {
            return Ok(());
        }
        write!(self.out, "\r\x1b[K")?;
        if self.rendered_rows > 1 {
            write!(self.out, "\x1b[{}A", self.rendered_rows - 1)?;
        }
        for index in 0..self.rendered_rows {
            write!(self.out, "\r\x1b[K")?;
            if index + 1 < self.rendered_rows {
                writeln!(self.out)?;
            }
        }
        self.out.flush()?;
        self.active = false;
        self.rendered_rows = 0;
        Ok(())
    }

    fn ordered_rows(&self) -> Vec<&TtyProgressRow> {
        let mut rows = self.rows.iter().collect::<Vec<_>>();
        rows.sort_by_key(|row| update::dashboard_stage_index(row.stage));
        rows
    }

    fn live_rows(&self) -> Vec<RenderedTtyRow> {
        self.ordered_rows()
            .into_iter()
            .map(|row| {
                let complete = row.touched && progress_complete(row);
                let status = if complete {
                    "ok".to_owned()
                } else if row.touched {
                    spinner_frame(row.done).to_owned()
                } else {
                    "pending".to_owned()
                };
                let elapsed_ms = row.elapsed_ms.unwrap_or_else(|| {
                    row.started
                        .map_or(0, |started| started.elapsed().as_millis())
                });
                RenderedTtyRow {
                    status,
                    detail: format!("{} {}", row.label, progress_bar(row.done, row.total, 8)),
                    elapsed: elapsed_label_ms(elapsed_ms),
                }
            })
            .collect()
    }
}

struct TtyProgressStage {
    stage: &'static str,
    total: usize,
}

struct RenderedTtyRow {
    status: String,
    detail: String,
    elapsed: String,
}

impl<W> Drop for TtyProgress<'_, W>
where
    W: Write,
{
    fn drop(&mut self) {
        let _ = self.clear_unfinished();
    }
}

impl<W> update::Progress for TtyProgress<'_, W>
where
    W: Write,
{
    fn phase(&mut self, phase: update::Phase<'_>) -> Result<()> {
        if phase.total == 0 || phase.status != "running" {
            return Ok(());
        }
        self.progress_row(phase.label, phase.key, phase.done, phase.total)
    }

    fn item(&mut self, _group: &'static str, _item: &update::Item) -> Result<()> {
        Ok(())
    }
}

fn prune_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let mut prune_options = PruneOptions {
        quiet: options.quiet,
        ..PruneOptions::default()
    };
    for arg in args {
        match arg.as_str() {
            "-y" => prune_options.yes = true,
            "--dry-run" => prune_options.dry_run = true,
            other => {
                writeln!(stderr, "error: unknown prune option '{other}'")?;
                return Ok(2);
            }
        }
    }

    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    let raw_entries = config::load_dir(&roots.conf_dir)?;
    let entries = parse_entries(&raw_entries, "");
    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
    let manifest_path = manifest::path(&roots.state_dir);
    let manifest = manifest::read(&manifest_path)?;
    let entries =
        resolve_github_entries(&entries, &roots, Some(&manifest), &env, &env_vars, options)?;
    let hooks = custom_probe();
    let detected = prune::run(
        &entries,
        &manifest,
        &manifest_path,
        &roots,
        &hooks,
        PruneOptions {
            dry_run: true,
            ..prune_options
        },
    )?;

    if detected.guarded_all_orphans {
        writeln!(
            stderr,
            "warning: no deps in config but {} in manifest — all would be orphaned",
            detected.orphans.len()
        )?;
        writeln!(stderr, "  If intentional, re-run with -y")?;
        return Ok(1);
    }
    if detected.orphans.is_empty() {
        if !prune_options.quiet {
            writeln!(stdout, "No orphaned deps found.")?;
        }
        return Ok(0);
    }
    if prune_options.quiet && !prune_options.yes {
        return Ok(0);
    }

    write_prune_orphans(&detected.orphans, false, stdout)?;
    if prune_options.dry_run {
        writeln!(stdout, "Dry run — nothing removed.")?;
        return Ok(0);
    }

    if !prune_options.yes && !confirm_prune(stdout)? {
        writeln!(stdout, "Aborted.")?;
        return Ok(0);
    }

    let manifest = manifest::read(&manifest_path)?;
    let summary = prune::run(
        &entries,
        &manifest,
        &manifest_path,
        &roots,
        &hooks,
        PruneOptions {
            yes: true,
            dry_run: false,
            quiet: false,
        },
    )?;
    write_prune_results(&summary.removed, stdout, stderr)?;
    Ok(if summary.has_errors() { 1 } else { 0 })
}

fn self_update_cmd<W, E>(
    args: &[String],
    options: &ParsedOptions,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    if let Some(arg) = args.first() {
        writeln!(stderr, "error: unknown self-update argument '{arg}'")?;
        return Ok(2);
    }

    let dir = shdeps_install_dir()?;
    match self_update::target(&dir)? {
        Target::SourceCheckout => {
            let summary = self_update::source_checkout(&dir, &Process)?;
            write_source_self_update(&summary, stderr)?;
            Ok(summary.exit_code())
        }
        Target::ReleaseArchive(metadata) => {
            match self_update::release_archive(&dir, &metadata, &ProcessEnv, &Process, &Curl) {
                Ok(summary) => {
                    write_release_self_update(&summary, options.quiet, stdout, stderr)?;
                    Ok(summary.exit_code())
                }
                Err(error) => {
                    writeln!(stderr, "shdeps: self-update failed ({error})")?;
                    Ok(1)
                }
            }
        }
        Target::Unsupported { reason } => {
            writeln!(stderr, "{reason}")?;
            Ok(1)
        }
    }
}

fn dep_root<W>(target: &str, options: &ParsedOptions, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    run_path_lookup(
        dep_path::root(
            target,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn dep_path<W>(target: &str, rel: &str, options: &ParsedOptions, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    run_path_lookup(
        dep_path::path(
            target,
            rel,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn dep_file<W>(target: &str, rel: &str, options: &ParsedOptions, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    run_path_lookup(
        dep_path::file(
            target,
            rel,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn parse_entries(raw_entries: &[String], pkg_mgr: &str) -> Vec<Entry> {
    let pkg_mgr = if pkg_mgr.is_empty() {
        None
    } else {
        Some(pkg_mgr)
    };
    raw_entries
        .iter()
        .map(|entry| config::parse_entry(entry, pkg_mgr))
        .collect()
}

fn resolve_github_entries(
    entries: &[Entry],
    roots: &runtime::Roots,
    manifest: Option<&Manifest>,
    env: &crate::platform::RuntimeEnv,
    env_vars: &BTreeMap<String, String>,
    options: &ParsedOptions,
) -> Result<Vec<Entry>> {
    resolve_github_entries_with_options(
        entries,
        roots,
        manifest,
        env,
        env_vars,
        github_options(options),
    )
}

fn resolve_github_entry(
    entry: &Entry,
    roots: &runtime::Roots,
    manifest: Option<&Manifest>,
    env: &crate::platform::RuntimeEnv,
    env_vars: &BTreeMap<String, String>,
    options: &ParsedOptions,
) -> Result<Entry> {
    let context = github_method::Context {
        roots,
        manifest,
        env,
        env_vars,
        runner: &Process,
        client: &Curl,
    };
    github_method::resolve_entry(entry, &context, github_options(options))
}

fn resolve_github_entries_with_options(
    entries: &[Entry],
    roots: &runtime::Roots,
    manifest: Option<&Manifest>,
    env: &crate::platform::RuntimeEnv,
    env_vars: &BTreeMap<String, String>,
    options: github_method::Options,
) -> Result<Vec<Entry>> {
    let context = github_method::Context {
        roots,
        manifest,
        env,
        env_vars,
        runner: &Process,
        client: &Curl,
    };
    github_method::resolve_entries_with_progress(
        entries,
        &context,
        options,
        jobs::github_max(env_vars),
        |_done, _total| Ok(()),
    )
}

fn resolve_github_entries_with_progress_sink(
    entries: &[Entry],
    roots: &runtime::Roots,
    manifest: Option<&Manifest>,
    env: &crate::platform::RuntimeEnv,
    env_vars: &BTreeMap<String, String>,
    options: github_method::Options,
    progress: &mut dyn update::Progress,
) -> Result<Vec<Entry>> {
    let context = github_method::Context {
        roots,
        manifest,
        env,
        env_vars,
        runner: &Process,
        client: &Curl,
    };
    github_method::resolve_entries_with_progress(
        entries,
        &context,
        options,
        jobs::github_max(env_vars),
        |done, total| {
            progress.phase(update::running_phase(
                update::PHASE_GITHUB_METHODS,
                done,
                total,
            ))
        },
    )
}

fn resolve_github_entries_with_progress<W>(
    entries: &[Entry],
    roots: &runtime::Roots,
    manifest: Option<&Manifest>,
    env: &crate::platform::RuntimeEnv,
    env_vars: &BTreeMap<String, String>,
    options: github_method::Options,
    stdout: &mut W,
) -> Result<Vec<Entry>>
where
    W: Write,
{
    let context = github_method::Context {
        roots,
        manifest,
        env,
        env_vars,
        runner: &Process,
        client: &Curl,
    };
    let mut progress = JsonlProgress { out: stdout };
    github_method::resolve_entries_with_progress(
        entries,
        &context,
        options,
        jobs::github_max(env_vars),
        |done, total| {
            update::Progress::phase(
                &mut progress,
                update::running_phase(update::PHASE_GITHUB_METHODS, done, total),
            )
        },
    )
}

fn github_options(options: &ParsedOptions) -> github_method::Options {
    github_method::Options {
        force: runtime::force(&ProcessEnv, &options.overrides),
        reinstall: runtime::reinstall(&ProcessEnv, &options.overrides),
        remote_ttl: remote_ttl(),
        ..github_method::Options::default()
    }
}

fn github_options_from_update(options: UpdateOptions) -> github_method::Options {
    github_method::Options {
        force: options.force,
        reinstall: options.reinstall,
        now: options.now,
        remote_ttl: options.remote_ttl,
    }
}

fn write_list<W>(statuses: &[DependencyStatus], stdout: &mut W) -> Result<()>
where
    W: Write,
{
    let name_width = statuses
        .iter()
        .map(|status| status.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let method_width = statuses
        .iter()
        .map(|status| status.method.len())
        .max()
        .unwrap_or(6)
        .max(6);

    writeln!(
        stdout,
        "{:<name_width$} {:<method_width$} {:<12} DETAILS",
        "NAME", "METHOD", "STATUS"
    )?;
    writeln!(
        stdout,
        "{:<name_width$} {:<method_width$} {:<12} -------",
        "----", "------", "------"
    )?;

    for status in statuses {
        let (state, detail) = list_fields(&status.state);
        writeln!(
            stdout,
            "{:<name_width$} {:<method_width$} {:<12} {}",
            status.name, status.method, state, detail
        )?;
    }
    Ok(())
}

fn list_fields(state: &State) -> (&'static str, String) {
    match state {
        State::Installed { detail } => ("installed", detail.clone().unwrap_or_default()),
        State::Missing => ("missing", String::new()),
        State::Skipped(SkipReason::Platform) => ("skipped", "(platform)".to_owned()),
        State::Skipped(SkipReason::Host) => ("skipped", "(host)".to_owned()),
        State::Skipped(SkipReason::PackageManager) => ("skipped", "(pkg manager)".to_owned()),
    }
}

fn write_check<W>(status: &DependencyStatus, stdout: &mut W) -> Result<()>
where
    W: Write,
{
    match &status.state {
        State::Installed { detail } => {
            if let Some(detail) = detail {
                writeln!(stdout, "{}: installed ({detail})", status.name)?;
            } else {
                writeln!(stdout, "{}: installed", status.name)?;
            }
        }
        State::Missing => {
            writeln!(stdout, "{}: not installed", status.name)?;
        }
        State::Skipped(SkipReason::Platform) => {
            writeln!(stdout, "{}: skipped (platform mismatch)", status.name)?;
        }
        State::Skipped(SkipReason::Host) => {
            writeln!(stdout, "{}: skipped (host mismatch)", status.name)?;
        }
        State::Skipped(SkipReason::PackageManager) => {
            writeln!(stdout, "{}: skipped (pkg manager)", status.name)?;
        }
    }
    Ok(())
}

fn write_update_summary<W, E>(
    summary: &update::Summary,
    entries: &[Entry],
    active_count: usize,
    quiet: bool,
    verbose: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    if verbose && !quiet {
        return write_verbose_update_summary(summary, entries, active_count, stdout, stderr);
    }

    let mut item_failures = std::collections::BTreeSet::new();
    for item in &summary.items {
        if item.failed {
            item_failures.insert(item.name.as_str());
            write_row(stderr, "failed", &format!("{}: {}", item.name, item.detail))?;
        }
    }

    for name in &summary.failed {
        if !item_failures.contains(name.as_str()) {
            write_row(stderr, "failed", &format!("{name}: post hook failed"))?;
        }
    }

    for name in &summary.leftovers {
        write_row(
            stderr,
            "warning",
            &format!("{name}: old-method cleanup left artifacts behind"),
        )?;
    }

    if !quiet {
        write_normal_group_summaries(summary, entries, stdout)?;
        let counts = update_counts(summary, active_count);
        if counts.failed == 0 {
            write_row(
                stdout,
                update_status(counts),
                &update_ok_summary(counts.changed, counts.current, counts.skipped),
            )?;
        } else {
            write_row(stderr, "failed", &format!("{} failed", counts.failed))?;
        }
    }

    Ok(())
}

fn write_update_terminal_summary<W, E>(
    summary: &update::Summary,
    entries: &[Entry],
    quiet: bool,
    verbose: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    if verbose && !quiet {
        return write_verbose_update_terminal_summary(summary, entries, stdout, stderr);
    }

    let mut item_failures = std::collections::BTreeSet::new();
    for item in &summary.items {
        if item.failed {
            item_failures.insert(item.name.as_str());
            write_row(stderr, "failed", &format!("{}: {}", item.name, item.detail))?;
        }
    }

    for name in &summary.failed {
        if !item_failures.contains(name.as_str()) {
            write_row(stderr, "failed", &format!("{name}: post hook failed"))?;
        }
    }

    for name in &summary.leftovers {
        write_row(
            stderr,
            "warning",
            &format!("{name}: old-method cleanup left artifacts behind"),
        )?;
    }

    Ok(())
}

fn write_verbose_update_terminal_summary<W, E>(
    summary: &update::Summary,
    entries: &[Entry],
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    let mut item_failures = std::collections::BTreeSet::new();
    for group in update::dashboard_group_order().iter().copied() {
        let items = summary
            .items
            .iter()
            .filter(|item| dashboard_group_for_name(entries, &item.name) == group)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        write_group(stdout, update::label_for_display_group(group))?;
        for item in items {
            if item.failed {
                item_failures.insert(item.name.as_str());
                write_nested_row(stderr, "failed", &format!("{}: {}", item.name, item.detail))?;
            } else {
                write_nested_row(
                    stdout,
                    item_status(item),
                    &format!("{}: {}", item.name, item.detail),
                )?;
            }
        }
    }

    for name in &summary.failed {
        if !item_failures.contains(name.as_str()) {
            write_row(stderr, "failed", &format!("{name}: post hook failed"))?;
        }
    }

    for name in &summary.leftovers {
        write_row(
            stderr,
            "warning",
            &format!("{name}: old-method cleanup left artifacts behind"),
        )?;
    }

    Ok(())
}

fn terminal_progress_plan(
    entries: &[Entry],
    env: &crate::platform::RuntimeEnv,
) -> Vec<TtyProgressStage> {
    let mut totals = BTreeMap::<&'static str, usize>::new();
    for entry in entries.iter().filter(|entry| update::active(entry, env)) {
        if entry.method == method::GITHUB {
            *totals.entry(update::DASH_METHOD_RESOLUTION).or_insert(0) += 1;
            *totals.entry(update::DASH_GITHUB).or_insert(0) += 1;
            continue;
        }

        if method::is_github_config(&entry.method) {
            *totals.entry(update::DASH_GITHUB).or_insert(0) += 1;
            continue;
        }

        let stage = update::display_group_for_update_group(update::group_for_method(&entry.method));
        *totals.entry(stage).or_insert(0) += 1;
    }

    update::dashboard_stage_order()
        .iter()
        .filter_map(|stage| {
            totals
                .get(stage)
                .copied()
                .filter(|total| *total > 0)
                .map(|total| TtyProgressStage { stage, total })
        })
        .collect()
}

fn write_normal_group_summaries<W>(
    summary: &update::Summary,
    entries: &[Entry],
    stdout: &mut W,
) -> Result<()>
where
    W: Write,
{
    for group in update::dashboard_group_order().iter().copied() {
        let items = summary
            .items
            .iter()
            .filter(|item| dashboard_group_for_name(entries, &item.name) == group)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        let counts = update_counts_for_items(&items);
        let status = if counts.failed > 0 {
            "failed"
        } else if counts.changed > 0 {
            "changed"
        } else {
            "ok"
        };
        write_row(
            stdout,
            status,
            &format!(
                "{}: {}",
                update::label_for_display_group(group),
                update_count_summary(counts)
            ),
        )?;
        for item in items {
            if item.changed && !item.failed {
                write_nested_row(
                    stdout,
                    "changed",
                    &format!("{}: {}", item.name, item.detail),
                )?;
            }
        }
    }
    Ok(())
}

fn write_verbose_update_summary<W, E>(
    summary: &update::Summary,
    entries: &[Entry],
    active_count: usize,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    let mut item_failures = std::collections::BTreeSet::new();
    for group in update::dashboard_group_order().iter().copied() {
        let items = summary
            .items
            .iter()
            .filter(|item| dashboard_group_for_name(entries, &item.name) == group)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        write_group(stdout, update::label_for_display_group(group))?;
        for item in items {
            if item.failed {
                item_failures.insert(item.name.as_str());
                write_nested_row(stderr, "failed", &format!("{}: {}", item.name, item.detail))?;
            } else {
                write_nested_row(
                    stdout,
                    item_status(item),
                    &format!("{}: {}", item.name, item.detail),
                )?;
            }
        }
    }

    for name in &summary.failed {
        if !item_failures.contains(name.as_str()) {
            write_row(stderr, "failed", &format!("{name}: post hook failed"))?;
        }
    }

    for name in &summary.leftovers {
        write_row(
            stderr,
            "warning",
            &format!("{name}: old-method cleanup left artifacts behind"),
        )?;
    }

    let counts = update_counts(summary, active_count);
    if counts.failed == 0 {
        write_row(
            stdout,
            update_status(counts),
            &update_ok_summary(counts.changed, counts.current, counts.skipped),
        )?;
    } else {
        write_row(stderr, "failed", &format!("{} failed", counts.failed))?;
    }
    Ok(())
}

fn group_for_name(entries: &[Entry], name: &str) -> &'static str {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| update::group_for_method(&entry.method))
        .unwrap_or(update::GROUP_OTHER)
}

fn dashboard_group_for_name(entries: &[Entry], name: &str) -> &'static str {
    update::display_group_for_update_group(group_for_name(entries, name))
}

fn dashboard_elapsed_ms(summary: &update::Summary, group: &str) -> u128 {
    summary
        .groups
        .iter()
        .filter(|summary| update::display_group_for_update_group(summary.group) == group)
        .map(|summary| summary.elapsed_ms)
        .max()
        .unwrap_or(0)
}

fn write_update_summary_jsonl<W>(
    summary: &update::Summary,
    entries: &[Entry],
    active_count: usize,
    progress: &mut JsonlProgress<'_, W>,
) -> Result<()>
where
    W: Write,
{
    write_group_summaries_jsonl(summary, entries, progress)?;
    let counts = update_counts(summary, active_count);
    let status = update_status(counts);
    progress.event(json!({
        "event": "summary",
        "status": status,
        "changed": counts.changed,
        "current": counts.current,
        "skipped": counts.skipped,
        "failed": counts.failed,
    }))
}

fn write_group_summaries_jsonl<W>(
    summary: &update::Summary,
    entries: &[Entry],
    progress: &mut JsonlProgress<'_, W>,
) -> Result<()>
where
    W: Write,
{
    let elapsed = summary
        .groups
        .iter()
        .map(|group| (group.group, group.elapsed_ms))
        .collect::<BTreeMap<_, _>>();
    for group in update::update_group_order().iter().copied() {
        let items = summary
            .items
            .iter()
            .filter(|item| group_for_name(entries, &item.name) == group)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        let counts = update_counts_for_items(&items);
        progress.event(json!({
            "event": "group_summary",
            "group": group,
            "label": update::label_for_display_group(update::display_group_for_update_group(group)),
            "status": update_status(counts),
            "changed": counts.changed,
            "current": counts.current,
            "skipped": counts.skipped,
            "failed": counts.failed,
            "elapsed_ms": elapsed.get(group).copied().unwrap_or(0),
        }))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateCounts {
    changed: usize,
    current: usize,
    skipped: usize,
    failed: usize,
}

fn update_counts(summary: &update::Summary, active_count: usize) -> UpdateCounts {
    let changed = summary
        .items
        .iter()
        .filter(|item| item.status == update::ItemStatus::Changed)
        .count();
    let skipped = summary
        .items
        .iter()
        .filter(|item| item.status == update::ItemStatus::Skipped)
        .count();
    let failed = summary
        .failed
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let current = active_count.saturating_sub(changed + skipped + failed);
    UpdateCounts {
        changed,
        current,
        skipped,
        failed,
    }
}

fn update_counts_for_items(items: &[&update::Item]) -> UpdateCounts {
    let changed = items
        .iter()
        .filter(|item| item.status == update::ItemStatus::Changed)
        .count();
    let skipped = items
        .iter()
        .filter(|item| item.status == update::ItemStatus::Skipped)
        .count();
    let failed = items
        .iter()
        .filter(|item| item.status == update::ItemStatus::Failed)
        .count();
    let current = items.len().saturating_sub(changed + skipped + failed);
    UpdateCounts {
        changed,
        current,
        skipped,
        failed,
    }
}

fn update_count_summary(counts: UpdateCounts) -> String {
    if counts.failed > 0 {
        let mut parts = vec![format!("{} failed", counts.failed)];
        if counts.changed > 0 {
            parts.push(format!("{} changed", counts.changed));
        }
        if counts.current > 0 {
            parts.push(format!("{} current", counts.current));
        }
        if counts.skipped > 0 {
            parts.push(format!("{} skipped", counts.skipped));
        }
        parts.join(", ")
    } else {
        update_ok_summary(counts.changed, counts.current, counts.skipped)
    }
}

fn item_status(item: &update::Item) -> &'static str {
    match item.status {
        update::ItemStatus::Changed => "changed",
        update::ItemStatus::Skipped => "skipped",
        update::ItemStatus::Failed => "failed",
        update::ItemStatus::Current | update::ItemStatus::Pending => "ok",
    }
}

fn update_status(counts: UpdateCounts) -> &'static str {
    if counts.failed > 0 {
        "failed"
    } else if counts.changed > 0 {
        "changed"
    } else {
        "ok"
    }
}

fn update_ok_summary(changed: usize, current: usize, skipped: usize) -> String {
    let mut parts = Vec::new();
    if changed > 0 {
        parts.push(format!("{changed} changed"));
    }
    if current > 0 || parts.is_empty() {
        parts.push(format!("{current} current"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    parts.join(", ")
}

fn write_prune_orphans<W>(
    orphans: &[manifest::ManifestEntry],
    include_hint: bool,
    stdout: &mut W,
) -> Result<()>
where
    W: Write,
{
    write_heading(stdout, "Warnings")?;
    let noun = if orphans.len() == 1 { "dep" } else { "deps" };
    write_row(
        stdout,
        "warning",
        &format!("{} orphaned {noun} no longer in config", orphans.len()),
    )?;
    for orphan in orphans {
        write_row(
            stdout,
            "detail",
            &format!("{} ({})", orphan.name, orphan.method),
        )?;
    }
    if include_hint {
        write_row(
            stdout,
            "hint",
            "run `shdeps prune` to remove orphaned artifacts",
        )?;
    }
    Ok(())
}

fn write_prune_orphans_jsonl<W>(
    orphans: &[manifest::ManifestEntry],
    progress: &mut JsonlProgress<'_, W>,
) -> Result<()>
where
    W: Write,
{
    let noun = if orphans.len() == 1 { "dep" } else { "deps" };
    progress.event(json!({
        "event": "warning",
        "status": "warning",
        "detail": format!("{} orphaned {noun} no longer in config", orphans.len()),
    }))?;
    for orphan in orphans {
        progress.event(json!({
            "event": "detail",
            "status": "detail",
            "detail": format!("{} ({})", orphan.name, orphan.method),
        }))?;
    }
    progress.event(json!({
        "event": "hint",
        "status": "hint",
        "detail": "run `shdeps prune` to remove orphaned artifacts",
    }))?;
    Ok(())
}

fn write_heading<W>(stdout: &mut W, label: &str) -> Result<()>
where
    W: Write,
{
    writeln!(stdout, "{}{label}{}", color("heading"), color("reset"))?;
    Ok(())
}

fn write_group<W>(stdout: &mut W, label: &str) -> Result<()>
where
    W: Write,
{
    writeln!(stdout, "  {}{label}{}", color("detail"), color("reset"))?;
    Ok(())
}

fn write_row<W>(stdout: &mut W, status: &str, detail: &str) -> Result<()>
where
    W: Write,
{
    writeln!(
        stdout,
        "  {}{status:<8}{} {detail}",
        color(status),
        color("reset")
    )?;
    Ok(())
}

fn write_nested_row<W>(stdout: &mut W, status: &str, detail: &str) -> Result<()>
where
    W: Write,
{
    writeln!(
        stdout,
        "    {}{status:<8}{} {detail}",
        color(status),
        color("reset")
    )?;
    Ok(())
}

fn progress_label(label: &str) -> String {
    format!("{label:<18}")
}

fn progress_done(row: &TtyProgressRow) -> usize {
    let done = if row.stage == update::DASH_GITHUB {
        let release_done = [
            update::PHASE_GITHUB_RELEASE_METADATA,
            update::PHASE_GITHUB_RELEASE_VERSIONS,
            update::PHASE_GITHUB_RELEASE_INSTALLS,
        ]
        .into_iter()
        .filter_map(|component| row.components.get(component).map(|(done, _)| *done))
        .max()
        .unwrap_or(0);
        let repo_done = row
            .components
            .get(update::PHASE_GITHUB_REPOS)
            .map(|(done, _)| *done)
            .unwrap_or(0);
        release_done + repo_done
    } else {
        row.components
            .values()
            .map(|(component_done, _)| *component_done)
            .sum()
    };

    done.min(row.total)
}

fn progress_complete(row: &TtyProgressRow) -> bool {
    if row.total == 0 || row.done < row.total {
        return false;
    }
    if row.stage != update::DASH_GITHUB {
        return true;
    }

    let release_expected = [
        update::PHASE_GITHUB_RELEASE_METADATA,
        update::PHASE_GITHUB_RELEASE_VERSIONS,
        update::PHASE_GITHUB_RELEASE_INSTALLS,
    ]
    .into_iter()
    .filter_map(|component| row.components.get(component).map(|(_, total)| *total))
    .max()
    .unwrap_or(0);
    let release_done = row
        .components
        .get(update::PHASE_GITHUB_RELEASE_INSTALLS)
        .map(|(done, _)| *done)
        .unwrap_or(0);

    release_expected == 0 || release_done >= release_expected
}

fn progress_done_detail(stage: &str, label: &str, total: usize) -> String {
    let label = label.trim();
    let action = match stage {
        update::DASH_METHOD_RESOLUTION => "resolved",
        _ => "checked",
    };
    format!("{label}: {total} {action}")
}

fn progress_bar(done: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let filled = ((done.saturating_mul(width)) / total).min(width);
    let empty = width.saturating_sub(filled);
    let digits = total.to_string().len().max(done.to_string().len());
    format!(
        "[{}{}] {:>digits$}/{total}",
        "━".repeat(filled),
        "·".repeat(empty),
        done
    )
}

fn spinner_frame(done: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[done % FRAMES.len()]
}

#[cfg(test)]
fn tty_row(status: &str, detail: &str, elapsed: &str) -> String {
    tty_row_with_color(status, detail, elapsed, false)
}

fn tty_row_with_color(status: &str, detail: &str, elapsed: &str, enabled: bool) -> String {
    let color_status = match status {
        "pending" => "detail",
        "⠋" | "⠙" | "⠹" | "⠸" | "⠼" | "⠴" | "⠦" | "⠧" | "⠇" | "⠏" => "running",
        _ => status,
    };
    format!(
        "  {}{status:<8}{} {detail:<54} {elapsed:>6}",
        color_when(color_status, enabled),
        color_when("reset", enabled)
    )
}

fn elapsed_label_ms(ms: u128) -> String {
    format!("{}s", ms / 1000)
}

fn color(status: &str) -> &'static str {
    color_when(status, color_enabled())
}

fn color_when(status: &str, enabled: bool) -> &'static str {
    if !enabled {
        return "";
    }
    match status {
        "heading" => "\x1b[1;36m",
        "ok" => "\x1b[32m",
        "changed" => "\x1b[34m",
        "running" => "\x1b[36m",
        "skipped" => "\x1b[2m",
        "warning" => "\x1b[33m",
        "failed" => "\x1b[31m",
        "detail" | "hint" => "\x1b[2m",
        "reset" => "\x1b[0m",
        _ => "",
    }
}

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn write_prune_results<W, E>(items: &[prune::Item], stdout: &mut W, stderr: &mut E) -> Result<()>
where
    W: Write,
    E: Write,
{
    for item in items {
        match item.hook {
            Uninstall::MissingHook if item.entry.method == method::CUSTOM => writeln!(
                stderr,
                "  warning: {} hook file missing — manual cleanup may be needed",
                item.entry.name
            )?,
            Uninstall::MissingFunction if item.entry.method == method::CUSTOM => writeln!(
                stderr,
                "  warning: {} has no uninstall() hook — manual cleanup may be needed",
                item.entry.name
            )?,
            Uninstall::SourceFailed => writeln!(
                stderr,
                "  warning: failed to source hook for {}",
                item.entry.name
            )?,
            Uninstall::Failed => {
                writeln!(
                    stderr,
                    "  warning: {} uninstall hook failed",
                    item.entry.name
                )?;
            }
            Uninstall::MissingHook | Uninstall::MissingFunction | Uninstall::Removed => {}
        }

        if let Some(cleanup) = &item.cleanup {
            if cleanup.preserved_package {
                writeln!(
                    stderr,
                    "  {}: pkg dep — remove manually via system package manager",
                    item.entry.name
                )?;
            } else if item.cleanup_error.is_none() {
                writeln!(stdout, "  {} removed", item.entry.name)?;
            }
        }
        if let Some(error) = &item.cleanup_error {
            writeln!(
                stderr,
                "  warning: {} cleanup failed: {error}",
                item.entry.name
            )?;
        }
    }
    Ok(())
}

fn write_source_self_update<E>(summary: &self_update::Summary, stderr: &mut E) -> Result<()>
where
    E: Write,
{
    if summary.outcome == Outcome::PullFailed {
        writeln!(
            stderr,
            "shdeps: self-update failed (git pull --ff-only failed)"
        )?;
    }
    Ok(())
}

fn write_release_self_update<W, E>(
    summary: &self_update::ReleaseArchiveSummary,
    quiet: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    match &summary.outcome {
        ReleaseArchiveOutcome::Updated { tag } => {
            if !quiet {
                writeln!(stdout, "shdeps: updated to {tag}")?;
            }
        }
        ReleaseArchiveOutcome::Repaired { tag, missing } => {
            if !quiet {
                writeln!(stdout, "shdeps: repaired {tag} (restored {missing})")?;
            }
        }
        ReleaseArchiveOutcome::NoUpdate { current, .. } => {
            if !quiet {
                writeln!(stdout, "shdeps: no update available ({current})")?;
            }
        }
        ReleaseArchiveOutcome::NoSelectableRelease => {
            writeln!(stderr, "shdeps: no stable release available")?;
        }
        ReleaseArchiveOutcome::UnsupportedPlatform => {
            writeln!(
                stderr,
                "shdeps: no supported release artifact for this platform"
            )?;
        }
        ReleaseArchiveOutcome::NoSupportedArtifact {
            tag,
            platform_label,
        } => {
            writeln!(
                stderr,
                "shdeps: release {tag} has no artifact for {platform_label}"
            )?;
        }
    }
    Ok(())
}

fn write_update_source_self_update_result<E>(
    summary: &self_update::Summary,
    verbose: bool,
    stderr: &mut E,
) -> Result<()>
where
    E: Write,
{
    if !verbose {
        return Ok(());
    }

    match summary.outcome {
        Outcome::Unsupported => {
            writeln!(
                stderr,
                "shdeps: update self-check skipped; unsupported install"
            )?;
        }
        Outcome::DirtySkipped => {
            writeln!(stderr, "shdeps: update self-check skipped dirty checkout")?;
        }
        Outcome::Pulled => {
            writeln!(stderr, "shdeps: update self-check pulled source checkout")?;
        }
        Outcome::BuildFailed => {
            writeln!(
                stderr,
                "shdeps: update self-check pulled source checkout but build failed; continuing"
            )?;
        }
        Outcome::PullFailed => {
            writeln!(
                stderr,
                "shdeps: update self-check could not pull source checkout; continuing"
            )?;
        }
    }
    Ok(())
}

fn write_update_release_self_update_result<E>(
    summary: &self_update::ReleaseArchiveSummary,
    verbose: bool,
    stderr: &mut E,
) -> Result<()>
where
    E: Write,
{
    if !verbose {
        return Ok(());
    }

    match &summary.outcome {
        ReleaseArchiveOutcome::Updated { tag } => {
            writeln!(stderr, "shdeps: update self-check installed {tag}")?;
        }
        ReleaseArchiveOutcome::Repaired { tag, missing } => {
            writeln!(
                stderr,
                "shdeps: update self-check repaired {tag} (restored {missing})"
            )?;
        }
        ReleaseArchiveOutcome::NoUpdate { current, .. } => {
            writeln!(
                stderr,
                "shdeps: update self-check found no newer release ({current})"
            )?;
        }
        ReleaseArchiveOutcome::NoSelectableRelease => {
            writeln!(
                stderr,
                "shdeps: update self-check found no stable release; continuing"
            )?;
        }
        ReleaseArchiveOutcome::UnsupportedPlatform => {
            writeln!(
                stderr,
                "shdeps: update self-check has no supported artifact for this platform; continuing"
            )?;
        }
        ReleaseArchiveOutcome::NoSupportedArtifact {
            tag,
            platform_label,
        } => {
            writeln!(
                stderr,
                "shdeps: update self-check found no {platform_label} artifact in {tag}; continuing"
            )?;
        }
    }
    Ok(())
}

fn confirm_prune<W>(stdout: &mut W) -> Result<bool>
where
    W: Write,
{
    write!(stdout, "Remove? [y/N] ")?;
    stdout.flush()?;
    let mut reply = String::new();
    std::io::stdin().read_line(&mut reply)?;
    Ok(reply.trim().eq_ignore_ascii_case("y"))
}

fn check_exit_code(state: &State) -> i32 {
    match state {
        State::Missing => 1,
        State::Installed { .. } | State::Skipped(_) => 0,
    }
}

fn ensure_bin_dir_on_path(bin_dir: &Path) {
    let current = env::var_os("PATH").unwrap_or_else(default_path);
    let mut paths = vec![bin_dir.to_path_buf()];
    paths.extend(env::split_paths(&current).filter(|path| path != bin_dir));

    if let Ok(joined) = env::join_paths(paths) {
        // Bash prepends SHDEPS_BIN_DIR before update so newly installed tools
        // are immediately visible to later install methods and hooks in the
        // same run. The Rust CLI mutates only this process environment, right
        // before spawning those subprocess-backed phases, to preserve that
        // behavior without making every runner call rebuild PATH by hand.
        //
        // Rust 2024 marks environment mutation unsafe because concurrent reads
        // in other threads can race with the process-wide environment update.
        // `shdeps` reaches this path from the single-threaded CLI command
        // dispatcher before it starts hook/package subprocesses, so there are
        // no Rust threads in this process observing PATH while it changes.
        unsafe {
            env::set_var("PATH", joined);
        }
    }
}

fn default_path() -> std::ffi::OsString {
    // Real interactive shells virtually always provide PATH, but the test
    // harness intentionally starts commands with a scrubbed environment. Bash's
    // hook runner still has to find `bash`, `git`, package managers, and
    // language tools after shdeps prepends SHDEPS_BIN_DIR, so use the standard
    // Unix command locations as the compatibility fallback instead of turning
    // an absent PATH into "only SHDEPS_BIN_DIR".
    std::ffi::OsString::from("/usr/local/bin:/usr/bin:/bin")
}

fn remote_ttl() -> u64 {
    env::var("SHDEPS_REMOTE_TTL")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3600)
}

fn self_update_ttl() -> u64 {
    env::var("SHDEPS_SELF_UPDATE_TTL")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(86_400)
}

fn custom_probe() -> BashCustomProbe {
    shdeps_lib_path()
        .map(BashCustomProbe::new)
        .unwrap_or_else(BashCustomProbe::rust_prelude)
}

fn shdeps_lib_path() -> Option<PathBuf> {
    // `SHDEPS_LIB` is installer-facing: dotfiles and bootstrap paths use it to
    // pin a concrete sourceable library.
    if let Some(path) = env::var_os("SHDEPS_LIB").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }

    // The normal Rust path uses the generated hook prelude, not a nearby
    // checkout's `shdeps.sh`. That distinction matters for migration testing:
    // cargo integration-test binaries live under the repo and would otherwise
    // accidentally keep exercising the sourceable wrapper simply because it is
    // discoverable from the executable path.
    None
}

fn shdeps_install_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("SHDEPS_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let exe = env::current_exe()?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    if let Some(source_dir) = source_checkout_dir_from_target(dir) {
        return Ok(source_dir);
    }

    // Release installs put the Rust binary directly in SHDEPS_DIR, while older
    // source checkouts historically kept a CLI wrapper under bin/. Supporting
    // both shapes lets the same CLI entry point update converted installs and
    // developer checkouts without relying on a wrapper-specific env var.
    if dir.file_name().and_then(|name| name.to_str()) == Some("bin") {
        Ok(dir.parent().unwrap_or(dir).to_path_buf())
    } else {
        Ok(dir.to_path_buf())
    }
}

fn source_checkout_dir_from_target(dir: &Path) -> Option<PathBuf> {
    let profile = dir.file_name().and_then(|name| name.to_str())?;
    if profile != "debug" && profile != "release" {
        return None;
    }

    let target = dir.parent()?;
    if target.file_name().and_then(|name| name.to_str()) != Some("target") {
        return None;
    }

    let checkout = target.parent()?;
    // `std::env::current_exe()` resolves the `shdeps -> target/release/shdeps`
    // symlink on many Unix hosts. Without this checkout-shape correction,
    // running a source-built binary directly would try to self-update the
    // `target/release` directory instead of the repository that owns it.
    (checkout.join(".git").is_dir() && checkout.join("Cargo.toml").is_file())
        .then(|| checkout.to_path_buf())
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

fn dep_links_result<W>(
    result: Result<Vec<dep_links::DependencyLink>>,
    stdout: &mut W,
) -> Result<i32>
where
    W: Write,
{
    match result {
        Ok(links) => {
            dep_links::write_tsv(&links, stdout)?;
            Ok(0)
        }
        Err(Error::Resolve(error)) => Ok(error.exit_code()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{run, source_checkout_dir_from_target};
    use crate::config::Entry;
    use crate::update::{GroupSummary, Item, ItemReason, Summary};
    use crate::version;

    #[test]
    fn version_prints_embedded_public_version() {
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

    #[test]
    fn migrate_is_removed_from_user_facing_cli() {
        let (code, stdout, stderr) = run_capture(["migrate"]);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            "error: migrate has been removed from the user-facing CLI\nRun 'shdeps help' for usage.\n"
        );
    }

    #[test]
    fn api_version_is_machine_clean_abi_line() {
        let (code, stdout, stderr) = run_capture(["__api", "version"]);

        assert_eq!(code, 0);
        assert_eq!(stdout, "abi:1\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn api_force_uses_cli_override_as_predicate_status() {
        let (code, stdout, stderr) = run_capture(["--force", "__api", "force"]);

        assert_eq!(code, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn api_load_count_uses_config_parser_without_install_work() {
        let dir = temp_dir("load-count");
        fs::write(
            dir.join("deps.conf"),
            "# comment\nowner/tool.git github:repo\njq pkg\n",
        )
        .unwrap();

        let (code, stdout, stderr) = run_capture_vec(vec![
            "-c".to_owned(),
            dir.to_string_lossy().into_owned(),
            "__api".to_owned(),
            "load-count".to_owned(),
        ]);

        assert_eq!(code, 0);
        assert_eq!(stdout, "2\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn api_github_release_install_validates_required_arguments() {
        let (code, stdout, stderr) = run_capture(["__api", "github-release-install", "tool"]);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            "error: __api github-release-install requires a name and command\n"
        );
    }

    #[test]
    fn target_dir_executables_resolve_to_source_checkout_root() {
        let checkout = temp_dir("target-dir-source-root");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::write(
            checkout.join("Cargo.toml"),
            "[package]\nname = \"shdeps\"\n",
        )
        .unwrap();
        let release_dir = checkout.join("target/release");
        fs::create_dir_all(&release_dir).unwrap();

        assert_eq!(
            source_checkout_dir_from_target(&release_dir),
            Some(checkout)
        );
    }

    #[test]
    fn terminal_progress_bar_preserves_width_and_counts() {
        assert_eq!(super::progress_bar(0, 10, 8), "[········]  0/10");
        assert_eq!(super::progress_bar(5, 10, 8), "[━━━━····]  5/10");
        assert_eq!(super::progress_bar(10, 10, 8), "[━━━━━━━━] 10/10");
    }

    #[test]
    fn terminal_progress_labels_match_update_phases() {
        assert_eq!(
            super::progress_label("Resolve sources"),
            "Resolve sources   "
        );
        assert_eq!(super::progress_label("GitHub"), "GitHub            ");
        assert_eq!(
            crate::update::phase_spec(crate::update::PHASE_GITHUB_RELEASE_METADATA).dashboard_stage,
            crate::update::DASH_GITHUB
        );
        assert_eq!(
            crate::update::phase_spec(crate::update::PHASE_GITHUB_REPOS).dashboard_stage,
            crate::update::DASH_GITHUB
        );
    }

    #[test]
    fn terminal_progress_rows_include_elapsed_column() {
        assert_eq!(
            super::tty_row("running", "Custom            [━━━━····] 1/2", "12s"),
            "  running  Custom            [━━━━····] 1/2                          12s"
        );
    }

    #[test]
    fn terminal_summary_omits_aggregate_status_line() {
        let summary = Summary {
            items: vec![Item::current("jq", ItemReason::Installed, "current")],
            ..Summary::default()
        };
        let entries = vec![Entry {
            name: "jq".to_owned(),
            method: "pkg".to_owned(),
            cmd: "jq".to_owned(),
            aliases: String::new(),
            filter: String::new(),
        }];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        super::write_update_terminal_summary(
            &summary,
            &entries,
            false,
            false,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        assert!(String::from_utf8(stdout).unwrap().is_empty());
        assert!(String::from_utf8(stderr).unwrap().is_empty());
    }

    #[test]
    fn terminal_progress_finish_clears_dropped_rows() {
        let entries = vec![Entry {
            name: "widget".to_owned(),
            method: "custom".to_owned(),
            cmd: "widget".to_owned(),
            aliases: String::new(),
            filter: String::new(),
        }];
        let summary = Summary {
            items: vec![Item::current("widget", ItemReason::Installed, "current")],
            groups: vec![GroupSummary {
                group: "custom",
                elapsed_ms: 0,
            }],
            ..Summary::default()
        };
        let mut stdout = Vec::new();

        {
            let mut progress = super::TtyProgress::new(
                &mut stdout,
                vec![
                    super::TtyProgressStage {
                        stage: "github",
                        total: 1,
                    },
                    super::TtyProgressStage {
                        stage: "custom",
                        total: 1,
                    },
                ],
            );
            progress.start().unwrap();
            progress.finish(&summary, &entries, None).unwrap();
        }

        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("GitHub"));
        assert!(stdout.contains("Custom: 1 current"));
        assert!(stdout.ends_with("\n\r\x1b[K\x1b[1A\n"));
    }

    #[test]
    fn terminal_progress_footer_follows_finished_rows() {
        let entries = vec![Entry {
            name: "widget".to_owned(),
            method: "custom".to_owned(),
            cmd: "widget".to_owned(),
            aliases: String::new(),
            filter: String::new(),
        }];
        let summary = Summary {
            items: vec![Item::current("widget", ItemReason::Installed, "current")],
            groups: vec![GroupSummary {
                group: "custom",
                elapsed_ms: 0,
            }],
            ..Summary::default()
        };
        let mut stdout = Vec::new();

        {
            let mut progress = super::TtyProgress::new(
                &mut stdout,
                vec![super::TtyProgressStage {
                    stage: "custom",
                    total: 1,
                }],
            );
            progress.start().unwrap();
            progress
                .finish(&summary, &entries, Some("Done in 2s"))
                .unwrap();
        }

        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("  ok       Custom: 1 current"));
        assert!(stdout.contains("0s\n\r\x1b[KDone in 2s\n"));
        assert!(stdout.ends_with("Done in 2s\n"));
    }

    #[test]
    fn terminal_github_progress_does_not_regress_between_hidden_subphases() {
        let mut stdout = Vec::new();

        {
            let mut progress = super::TtyProgress::new(
                &mut stdout,
                vec![super::TtyProgressStage {
                    stage: "github",
                    total: 28,
                }],
            );
            progress.start().unwrap();
            progress
                .progress_row(
                    "GitHub",
                    crate::update::PHASE_GITHUB_RELEASE_METADATA,
                    15,
                    15,
                )
                .unwrap();
            let metadata_done = progress.rows[0].done;
            progress
                .progress_row(
                    "GitHub",
                    crate::update::PHASE_GITHUB_RELEASE_INSTALLS,
                    0,
                    15,
                )
                .unwrap();

            assert_eq!(metadata_done, 15);
            assert_eq!(
                progress.rows[0].done, metadata_done,
                "collapsed GitHub progress should not move backward when release installs start"
            );
            assert_eq!(progress.live_rows()[0].status, "⠴");

            progress
                .progress_row("GitHub", crate::update::PHASE_GITHUB_REPOS, 13, 13)
                .unwrap();
            assert_eq!(progress.rows[0].done, 28);
            assert_eq!(
                progress.live_rows()[0].status,
                "⠇",
                "the row should keep spinning until release install checks finish"
            );

            progress
                .progress_row(
                    "GitHub",
                    crate::update::PHASE_GITHUB_RELEASE_INSTALLS,
                    15,
                    15,
                )
                .unwrap();
            assert_eq!(progress.rows[0].done, 28);
            assert_eq!(progress.live_rows()[0].status, "ok");
        }
    }

    #[test]
    fn terminal_progress_plan_uses_one_github_release_row() {
        let entries = vec![Entry {
            name: "owner/repo".to_owned(),
            method: "github:release".to_owned(),
            cmd: "repo".to_owned(),
            aliases: String::new(),
            filter: String::new(),
        }];
        let env = crate::platform::RuntimeEnv::new("linux", "host");

        let stages = super::terminal_progress_plan(&entries, &env)
            .into_iter()
            .map(|stage| stage.stage)
            .collect::<Vec<_>>();

        assert_eq!(stages, vec!["github"]);
    }

    fn run_capture<const N: usize>(args: [&str; N]) -> (i32, String, String) {
        run_capture_vec(args.into_iter().map(str::to_owned).collect())
    }

    fn run_capture_vec(args: Vec<String>) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(args, &mut stdout, &mut stderr).expect("CLI run should not fail");

        (
            code,
            String::from_utf8(stdout).expect("stdout should be UTF-8"),
            String::from_utf8(stderr).expect("stderr should be UTF-8"),
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shdeps-cli-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
