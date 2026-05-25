//! CLI parsing and command dispatch for the Rust binary.
//!
//! This module owns user-facing command names, option parsing, exit codes, and
//! output text. Keeping formatting here prevents library modules from growing
//! incidental dependencies on terminal presentation.

use std::collections::BTreeMap;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api;
use crate::config::{self, Entry};
use crate::dep_path;
use crate::errors::Error;
use crate::hooks::{BashCustomProbe, Uninstall};
use crate::http::Curl;
use crate::jobs;
use crate::manifest;
use crate::process::{self, Process};
use crate::prune::{self, Options as PruneOptions};
use crate::runtime::{self, Overrides, ProcessEnv};
use crate::self_update::{self, Outcome, ReleaseArchiveOutcome, Target};
use crate::status::{self, Context as StatusContext, DependencyStatus, SkipReason, State};
use crate::update::{self, Context as UpdateContext, Options as UpdateOptions};
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
  update                 Install/update all dependencies
  self-update            Update shdeps itself (git pull, skips dirty trees)
  list                   List all configured dependencies with status
  check <name>           Check if a specific dependency is installed
  dep-root <name>        Print a configured dependency root directory
  dep-path <name> <rel>  Print a path below a configured dependency root
  dep-file <name> <rel>  Print a readable regular file below a dependency root
  prune                  Remove orphaned deps no longer in config
  version                Print shdeps version
  help                   Show this help message

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
        "self-update" => self_update_cmd(rest, stdout, stderr),
        "update" => update_cmd(rest, &parsed, stdout, stderr),
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
}

fn parse_options<W, E>(args: &[String], stdout: &mut W, stderr: &mut E) -> Result<ParseOutcome>
where
    W: Write,
    E: Write,
{
    let mut index = 0;
    let mut overrides = Overrides::default();
    let mut quiet = false;
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
            "-q" | "--quiet" | "-v" | "--verbose" | "-y" | "--dry-run" => {
                match arg.as_str() {
                    "-q" | "--quiet" => quiet = true,
                    "-v" | "--verbose" => {}
                    _ => {
                        writeln!(stderr, "error: unknown option '{arg}'")?;
                        writeln!(stderr, "Run 'shdeps help' for usage.")?;
                        return Ok(ParseOutcome::Exit(2));
                    }
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

    let manifest = manifest::read(&manifest::path(&roots.state_dir))?;
    let package_versions = if entries.iter().any(|entry| entry.method == "pkg") {
        process::package_versions(&Process, &pkg_mgr)
    } else {
        BTreeMap::new()
    };
    let custom = custom_probe();
    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
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
    let pkg_mgr = if probe_entry.method == "pkg" {
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
    let manifest = manifest::read(&manifest::path(&roots.state_dir))?;
    let package_versions = if entry.method == "pkg" {
        process::package_versions(&Process, &pkg_mgr)
    } else {
        BTreeMap::new()
    };
    let custom = custom_probe();
    let env = runtime::runtime_env(&ProcessEnv);
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
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    if let Some(arg) = args.first() {
        writeln!(stderr, "error: unknown update argument '{arg}'")?;
        return Ok(2);
    }

    let roots = runtime::roots(&ProcessEnv, &options.overrides);
    ensure_bin_dir_on_path(&roots.bin_dir);

    let pkg_mgr = process::detect_package_manager(&Process);
    let raw_entries = config::load_dir(&roots.conf_dir)?;
    let entries = parse_entries(&raw_entries, &pkg_mgr);
    if entries.is_empty() {
        writeln!(stdout, "No dependencies configured.")?;
        return Ok(0);
    }

    let manifest_path = manifest::path(&roots.state_dir);
    let manifest = manifest::read(&manifest_path)?;
    let hooks = custom_probe();
    let env = runtime::runtime_env(&ProcessEnv);
    let env_vars = std::env::vars().collect::<BTreeMap<_, _>>();
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
    let update_options = UpdateOptions {
        reinstall: runtime::reinstall(&ProcessEnv, &options.overrides),
        force: runtime::force(&ProcessEnv, &options.overrides),
        remote_ttl: remote_ttl(),
        ..UpdateOptions::default()
    };

    if !options.quiet {
        // This line looks cosmetic, but it is part of the legacy bootstrap
        // contract. dotfiles and humans both use it as the boundary between
        // repository sync/merge work and shdeps-managed dependency work, so
        // omitting it makes successful installs look like the update skipped
        // package/tool installation entirely.
        writeln!(stdout, "==> Installing/upgrading tools...")?;
    }

    let summary = update::run(&entries, &manifest, &context, update_options)?;
    write_update_summary(&summary, options.quiet, stdout, stderr)?;

    let manifest = manifest::read(&manifest_path)?;
    let orphans = manifest.orphans(&entries);
    if !orphans.is_empty() && !options.quiet {
        write_prune_orphans(&orphans, stderr)?;
        writeln!(stderr, "Run `shdeps prune` to remove orphaned artifacts.")?;
    }

    Ok(if summary.has_errors() { 1 } else { 0 })
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
    let manifest_path = manifest::path(&roots.state_dir);
    let manifest = manifest::read(&manifest_path)?;
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

    write_prune_orphans(&detected.orphans, stdout)?;
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

fn self_update_cmd<W, E>(args: &[String], stdout: &mut W, stderr: &mut E) -> Result<i32>
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
                    write_release_self_update(&summary, stdout, stderr)?;
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
    quiet: bool,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    let mut item_failures = std::collections::BTreeSet::new();
    for item in &summary.items {
        if item.failed {
            item_failures.insert(item.name.as_str());
            writeln!(stderr, "  {} failed: {}", item.name, item.detail)?;
        } else if item.changed && !quiet {
            writeln!(stdout, "  {}: {}", item.name, item.detail)?;
        }
    }

    for name in &summary.failed {
        if !item_failures.contains(name.as_str()) {
            writeln!(stderr, "  {name} post hook failed")?;
        }
    }

    for name in &summary.leftovers {
        writeln!(
            stderr,
            "  warning: {name} old-method cleanup left artifacts behind"
        )?;
    }

    Ok(())
}

fn write_prune_orphans<W>(orphans: &[manifest::ManifestEntry], stdout: &mut W) -> Result<()>
where
    W: Write,
{
    writeln!(
        stdout,
        "==> {} orphaned dep(s) no longer in config:",
        orphans.len()
    )?;
    for orphan in orphans {
        writeln!(stdout, "  {} ({})", orphan.name, orphan.method)?;
    }
    Ok(())
}

fn write_prune_results<W, E>(items: &[prune::Item], stdout: &mut W, stderr: &mut E) -> Result<()>
where
    W: Write,
    E: Write,
{
    for item in items {
        match item.hook {
            Uninstall::MissingHook if item.entry.method == "custom" => writeln!(
                stderr,
                "  warning: {} hook file missing — manual cleanup may be needed",
                item.entry.name
            )?,
            Uninstall::MissingFunction if item.entry.method == "custom" => writeln!(
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
    stdout: &mut W,
    stderr: &mut E,
) -> Result<()>
where
    W: Write,
    E: Write,
{
    match &summary.outcome {
        ReleaseArchiveOutcome::Updated { tag } => {
            writeln!(stdout, "shdeps: updated to {tag}")?;
        }
        ReleaseArchiveOutcome::NoUpdate { current, .. } => {
            writeln!(stdout, "shdeps: no update available ({current})")?;
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
        env::set_var("PATH", joined);
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

fn custom_probe() -> BashCustomProbe {
    shdeps_lib_path()
        .map(BashCustomProbe::new)
        .unwrap_or_else(BashCustomProbe::rust_prelude)
}

fn shdeps_lib_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("SHDEPS_RUST_LIB").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }

    // `SHDEPS_LIB` is installer-facing: dotfiles and bootstrap paths use it to
    // pin a concrete sourceable library. Keep the Rust test/transition override
    // above so test harnesses do not mutate the same variable that real
    // bootstrap consumers rely on.
    if let Some(path) = env::var_os("SHDEPS_LIB").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }

    // The normal Rust path uses the generated hook prelude, not a nearby
    // checkout's `shdeps.sh`. That distinction matters for migration testing:
    // cargo integration-test binaries live under the repo and would otherwise
    // accidentally keep exercising the legacy Bash library simply because it is
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

    // Release installs put the Rust binary directly in SHDEPS_DIR, while the
    // Bash source checkout keeps the wrapper at bin/shdeps. Supporting both
    // shapes lets the same CLI entry point update converted installs and
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{run, source_checkout_dir_from_target};
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
