//! Package phase for `shdeps update`.
//!
//! The package phase has enough policy to deserve its own module: package deps
//! run before every other method, unavailable packages are compatibility skips,
//! and batch failures retry one package at a time. Keeping that here prevents
//! the top-level update orchestrator from becoming another monolith.

use crate::Result;
use crate::config::{self, Entry};
use crate::manifest::{self, ManifestEntry};
use crate::method;
use crate::package_cache;
use crate::pkg;
use crate::process::{self, Output, Runner};
use crate::update::{
    Context, Item, ItemReason, Options, Progress, Summary, active, detail_with_action,
    verbose_enabled,
};
use std::collections::BTreeSet;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Queued {
    pub(crate) name: String,
    package: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SudoStatus {
    Available,
    UnavailableQuiet,
}

pub(crate) fn cache_status(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    count: usize,
    options: Options,
) -> Result<package_cache::Status> {
    if context.pkg_mgr.is_empty() {
        return Ok(package_cache::Status::Miss {
            reason: "no package manager".to_owned(),
        });
    }
    if let Some(reason) = cache_disabled(context, options) {
        return Ok(package_cache::Status::Miss {
            reason: reason.to_owned(),
        });
    }

    let inputs = package_cache::inputs(package_cache::InputSource {
        entries,
        roots: context.roots,
        env: context.env,
        manifest_path: context.manifest_path,
        pkg_mgr: context.pkg_mgr,
        env_vars: context.env_vars,
        runner: context.runner,
        count,
        force: options.force,
        reinstall: options.reinstall,
    })?;
    package_cache::current(&inputs)
}

pub(crate) fn write_cache(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    count: usize,
    options: Options,
) -> Result<()> {
    if context.pkg_mgr.is_empty() {
        return Ok(());
    }
    if cache_disabled(context, options).is_some() {
        return Ok(());
    }

    let inputs = package_cache::inputs(package_cache::InputSource {
        entries,
        roots: context.roots,
        env: context.env,
        manifest_path: context.manifest_path,
        pkg_mgr: context.pkg_mgr,
        env_vars: context.env_vars,
        runner: context.runner,
        count,
        force: options.force,
        reinstall: options.reinstall,
    })?;
    package_cache::write(&inputs)
}

fn cache_disabled<R: Runner>(context: &Context<'_, R>, options: Options) -> Option<&'static str> {
    if options.force || context.env_vars.get("SHDEPS_FORCE").map(String::as_str) == Some("1") {
        Some("force enabled")
    } else if options.reinstall
        || context.env_vars.get("SHDEPS_REINSTALL").map(String::as_str) == Some("1")
    {
        Some("reinstall enabled")
    } else if verbose_enabled(options, context.env_vars) {
        Some("verbose logging enabled")
    } else {
        None
    }
}

pub(crate) fn cached_items(entries: &[Entry], context: &Context<'_, impl Runner>) -> Vec<Item> {
    entries
        .iter()
        .filter(|entry| entry.method == method::PKG && active(entry, context.env))
        .map(|entry| {
            let resolved = config::resolve_override_for_runtime(
                &entry.name,
                &entry.aliases,
                Some(context.pkg_mgr),
                context.env.is_android(),
            );
            if resolved == "NONE" {
                Item::skipped(
                    entry.name.clone(),
                    ItemReason::PackageManagerOverride,
                    "skipped by package-manager override",
                )
            } else {
                Item::current(entry.name.clone(), ItemReason::Installed, "installed")
            }
        })
        .collect()
}

pub(crate) fn package_versions(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    options: Options,
    transitions: &BTreeSet<String>,
) -> std::collections::BTreeMap<String, String> {
    if !needs_package_version_snapshot(entries, context, options, transitions) {
        return std::collections::BTreeMap::new();
    }
    process::package_versions(context.runner, context.pkg_mgr)
}

pub(crate) fn sudo_status(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    package_versions: &std::collections::BTreeMap<String, String>,
    transitions: &BTreeSet<String>,
) -> Result<SudoStatus> {
    if !needs_package_work(entries, context, package_versions, transitions) {
        return Ok(SudoStatus::Available);
    }
    if pkg::Elevation::for_manager(context.pkg_mgr, context.env) == pkg::Elevation::Direct
        || context.pkg_mgr == "brew"
    {
        return Ok(SudoStatus::Available);
    }
    if context.env_vars.get("SHDEPS_QUIET").map(String::as_str) != Some("1") {
        return Ok(SudoStatus::Available);
    }
    if user_is_root(context.runner)? || sudo_noninteractive(context.runner)? {
        return Ok(SudoStatus::Available);
    }

    Ok(SudoStatus::UnavailableQuiet)
}

pub(crate) fn prepare(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    package_versions: &std::collections::BTreeMap<String, String>,
    transitions: &BTreeSet<String>,
    sudo: SudoStatus,
    progress: &mut dyn Progress,
) -> Result<()> {
    if !needs_package_work(entries, context, package_versions, transitions) {
        return Ok(());
    }
    if sudo == SudoStatus::UnavailableQuiet {
        return Ok(());
    }

    maybe_enable_epel(context, progress)?;
    refresh_metadata(context, progress)?;
    Ok(())
}

fn needs_package_version_snapshot(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    _options: Options,
    transitions: &BTreeSet<String>,
) -> bool {
    // Batch package versions are useful only when command lookup alone cannot
    // prove every active package dependency. Avoiding the manager-wide query on
    // the common "all commands are on PATH" path keeps forced updates snappy
    // while still collapsing the expensive fallback for fonts/data packages and
    // package-name/command-name mismatches.
    entries.iter().any(|entry| {
        if entry.method != method::PKG || !active(entry, context.env) {
            return false;
        }
        let resolved = config::resolve_override_for_runtime(
            &entry.name,
            &entry.aliases,
            Some(context.pkg_mgr),
            context.env.is_android(),
        );
        resolved != "NONE"
            && (transitions.contains(&entry.name) || !context.runner.exists(&entry.cmd))
    })
}

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    sudo: SudoStatus,
    queued: &mut Vec<Queued>,
    package_versions: &std::collections::BTreeMap<String, String>,
    transitioning: bool,
) -> Result<Item> {
    let resolved = config::resolve_override_for_runtime(
        &entry.name,
        &entry.aliases,
        Some(context.pkg_mgr),
        context.env.is_android(),
    );
    if resolved == "NONE" {
        return Ok(Item::skipped(
            entry.name.clone(),
            ItemReason::PackageManagerOverride,
            "skipped by package-manager override",
        ));
    }

    let installed = installed(entry, &resolved, context, package_versions, transitioning);
    if installed {
        manifest::upsert(
            context.manifest_path,
            ManifestEntry::new(&entry.name, method::PKG, &entry.cmd, ""),
        )?;
        let detail = if verbose_enabled(options, context.env_vars) {
            let version = process::dep_version(context.runner, &entry.cmd)
                .or_else(|| package_versions.get(&resolved).cloned());
            detail_with_action("installed", version.unwrap_or_default())
        } else {
            "installed".to_owned()
        };
        return Ok(if missing_command_needs_repair(entry, context) {
            Item::changed(entry.name.clone(), ItemReason::Installed, detail)
        } else {
            Item::current(entry.name.clone(), ItemReason::Installed, detail)
        });
    }

    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(&entry.name, method::PKG, &entry.cmd, ""),
    )?;

    if sudo == SudoStatus::UnavailableQuiet {
        return Ok(Item::skipped(
            entry.name.clone(),
            ItemReason::PackageSudoUnavailable,
            "sudo unavailable in quiet mode",
        ));
    }

    if !available(context.runner, context.pkg_mgr, &resolved)? {
        return Ok(Item::skipped(
            entry.name.clone(),
            ItemReason::PackageUnavailable,
            "not available",
        ));
    }

    queued.push(Queued {
        name: entry.name.clone(),
        package: resolved,
    });
    Ok(Item::pending(
        entry.name.clone(),
        ItemReason::PackageQueued,
        "queued",
    ))
}

pub(crate) fn flush(
    queued: &[Queued],
    context: &Context<'_, impl Runner>,
    sudo: SudoStatus,
    changed: &mut Vec<String>,
    summary: &mut Summary,
    progress: &mut dyn Progress,
) -> Result<()> {
    if queued.is_empty() {
        return Ok(());
    }
    if sudo == SudoStatus::UnavailableQuiet {
        return Err(io::Error::other("internal error: packages queued without quiet sudo").into());
    }

    let packages = queued
        .iter()
        .map(|item| item.package.clone())
        .collect::<Vec<_>>();
    let elevation = pkg::Elevation::for_manager(context.pkg_mgr, context.env);
    let Some(command) = pkg::install(context.pkg_mgr, &packages, elevation) else {
        return Ok(());
    };

    if run(context.runner, &command, progress)?.success {
        changed.extend(queued.iter().map(|item| item.name.clone()));
        return Ok(());
    }

    // Bash retries package installs one-at-a-time after a failed batch so a
    // single bad package does not block every other queued dependency. Keep the
    // same failure isolation here even though it costs extra subprocesses only
    // on the uncommon failure path.
    for item in queued {
        let single = vec![item.package.clone()];
        let Some(command) = pkg::install(context.pkg_mgr, &single, elevation) else {
            continue;
        };
        if run(context.runner, &command, progress)?.success {
            changed.push(item.name.clone());
        } else {
            summary.failed.push(item.name.clone());
        }
    }
    Ok(())
}

fn missing_command_needs_repair(entry: &Entry, context: &Context<'_, impl Runner>) -> bool {
    !context.runner.exists(&entry.cmd)
        && context
            .roots
            .hooks_dir
            .join(format!("{}.sh", entry.name))
            .is_file()
}

fn needs_package_work(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    package_versions: &std::collections::BTreeMap<String, String>,
    transitions: &BTreeSet<String>,
) -> bool {
    if context.pkg_mgr.is_empty() {
        return false;
    }

    entries.iter().any(|entry| {
        if entry.method != method::PKG || !active(entry, context.env) {
            return false;
        }

        let resolved = config::resolve_override_for_runtime(
            &entry.name,
            &entry.aliases,
            Some(context.pkg_mgr),
            context.env.is_android(),
        );
        resolved != "NONE"
            && !installed(
                entry,
                &resolved,
                context,
                package_versions,
                transitions.contains(&entry.name),
            )
    })
}

fn installed(
    entry: &Entry,
    package: &str,
    context: &Context<'_, impl Runner>,
    package_versions: &std::collections::BTreeMap<String, String>,
    transitioning: bool,
) -> bool {
    if transitioning {
        // A command owned by the old method must not satisfy the new package
        // provider: transition cleanup will remove that command afterward.
        return package_versions.contains_key(package)
            || process::package_installed(context.runner, package, context.pkg_mgr);
    }
    process::dep_exists_with_versions(
        context.runner,
        &entry.cmd,
        package,
        context.pkg_mgr,
        package_versions,
    )
}

fn user_is_root(runner: &impl Runner) -> Result<bool> {
    let output = runner.run("id", &["-u"], Some(process::VERSION_PROBE_TIMEOUT))?;
    Ok(output.success && output.stdout.trim() == "0")
}

fn sudo_noninteractive(runner: &impl Runner) -> Result<bool> {
    match runner.run(
        "sudo",
        &["-n", "true"],
        Some(process::VERSION_PROBE_TIMEOUT),
    ) {
        Ok(output) => Ok(output.success),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn maybe_enable_epel(
    context: &Context<'_, impl Runner>,
    progress: &mut dyn Progress,
) -> Result<()> {
    if context.pkg_mgr != "dnf"
        || context.env_vars.get("SHDEPS_AUTO_EPEL").map(String::as_str) != Some("1")
    {
        return Ok(());
    }

    // CentOS/RHEL-family machines often need EPEL for everyday CLI packages
    // such as ripgrep, and EPEL itself may depend on CodeReady Builder/CRB
    // being enabled first. This is intentionally best-effort like the Bash
    // implementation: if the host has no CRB, no EPEL package, or a locked-down
    // sudo policy, the per-dependency availability probe below still makes the
    // final skip/install decision without turning the whole update into a hard
    // failure.
    repair_dnf_optional_repo(context, progress)?;

    if best_effort_run_raw(context.runner, "rpm", &["-q", "epel-release"], progress)?
        .is_some_and(|output| output.success)
    {
        return Ok(());
    }

    let packages = vec!["epel-release".to_owned()];
    let Some(command) = pkg::install(
        "dnf",
        &packages,
        pkg::Elevation::for_manager("dnf", context.env),
    ) else {
        return Ok(());
    };
    let _ = best_effort_run(context.runner, &command, progress)?;
    Ok(())
}

fn repair_dnf_optional_repo(
    context: &Context<'_, impl Runner>,
    progress: &mut dyn Progress,
) -> Result<()> {
    for repo in ["crb", "powertools"] {
        let Some(output) = best_effort_run_raw(
            context.runner,
            "sudo",
            &["dnf", "config-manager", "--set-enabled", repo],
            progress,
        )?
        else {
            continue;
        };
        if output.success {
            break;
        }
    }
    Ok(())
}

fn refresh_metadata(context: &Context<'_, impl Runner>, progress: &mut dyn Progress) -> Result<()> {
    let Some(command) = pkg::refresh(
        context.pkg_mgr,
        pkg::Elevation::for_manager(context.pkg_mgr, context.env),
    ) else {
        return Ok(());
    };

    // Availability checks are only as good as the local package metadata. This
    // matters most in ephemeral CI containers: `apk add --no-cache cargo` can
    // install bootstrap tools without leaving an index for the later
    // `apk search -e jq` probe, and dnf needs an EPEL/metadata pass before
    // CentOS-only packages like ripgrep are visible. Refresh failures stay
    // nonfatal for Bash compatibility; the later availability probe still owns
    // the skip-vs-install decision for each dependency.
    let _ = best_effort_run(context.runner, &command, progress)?;
    Ok(())
}

fn available(runner: &impl Runner, mgr: &str, package: &str) -> Result<bool> {
    let Some(command) = pkg::available(mgr, package) else {
        return Ok(true);
    };
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner.run(
        &command.program,
        &args,
        Some(process::PACKAGE_PROBE_TIMEOUT),
    )?;
    Ok(pkg::available_ok(mgr, output.success, &output.stdout))
}

fn run(
    runner: &impl Runner,
    command: &pkg::CommandSpec,
    progress: &mut dyn Progress,
) -> Result<Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    run_raw(runner, &command.program, &args, progress)
}

fn run_raw(
    runner: &impl Runner,
    program: &str,
    args: &[&str],
    progress: &mut dyn Progress,
) -> Result<Output> {
    pause_for_prompt_if_needed(program, progress)?;
    runner.run(program, args, None).map_err(Into::into)
}

fn best_effort_run(
    runner: &impl Runner,
    command: &pkg::CommandSpec,
    progress: &mut dyn Progress,
) -> Result<Option<Output>> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    best_effort_run_raw(runner, &command.program, &args, progress)
}

fn best_effort_run_raw(
    runner: &impl Runner,
    program: &str,
    args: &[&str],
    progress: &mut dyn Progress,
) -> Result<Option<Output>> {
    // Package metadata preparation has always been advisory: it improves the
    // accuracy of availability probes, but a missing sudo/config-manager or a
    // transient repo refresh error must not hide the dependency-level result.
    // Install and availability calls still use `run_raw` so real install
    // failures are not silently swallowed.
    pause_for_prompt_if_needed(program, progress)?;
    Ok(runner.run(program, args, None).ok())
}

fn pause_for_prompt_if_needed(program: &str, progress: &mut dyn Progress) -> Result<()> {
    if program == "sudo" {
        progress.pause_for_prompt("waiting for sudo authentication")?;
    }
    Ok(())
}
