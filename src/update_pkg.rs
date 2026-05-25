//! Package phase for `shdeps update`.
//!
//! The package phase has enough policy to deserve its own module: package deps
//! run before every other method, unavailable packages are compatibility skips,
//! and batch failures retry one package at a time. Keeping that here prevents
//! the top-level update orchestrator from becoming another monolith.

use crate::config::{self, Entry};
use crate::manifest::{self, ManifestEntry};
use crate::package_cache;
use crate::pkg;
use crate::process::{self, Output, Runner};
use crate::update::{active, Context, Item, Options, Summary};
use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Queued {
    pub(crate) name: String,
    package: String,
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
    } else if context
        .env_vars
        .get("SHDEPS_LOG_LEVEL")
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(1)
        >= 2
    {
        Some("verbose logging enabled")
    } else {
        None
    }
}

pub(crate) fn cached_items(entries: &[Entry], context: &Context<'_, impl Runner>) -> Vec<Item> {
    entries
        .iter()
        .filter(|entry| entry.method == "pkg" && active(entry, context.env))
        .map(|entry| {
            let resolved =
                config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr));
            if resolved == "NONE" {
                Item {
                    name: entry.name.clone(),
                    changed: false,
                    failed: false,
                    detail: "skipped by package-manager override".to_owned(),
                }
            } else {
                Item {
                    name: entry.name.clone(),
                    changed: false,
                    failed: false,
                    detail: "installed".to_owned(),
                }
            }
        })
        .collect()
}

pub(crate) fn package_versions(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
) -> std::collections::BTreeMap<String, String> {
    if !needs_package_version_snapshot(entries, context) {
        return std::collections::BTreeMap::new();
    }
    process::package_versions(context.runner, context.pkg_mgr)
}

pub(crate) fn prepare(
    entries: &[Entry],
    context: &Context<'_, impl Runner>,
    package_versions: &std::collections::BTreeMap<String, String>,
) {
    if !needs_package_work(entries, context, package_versions) {
        return;
    }

    maybe_enable_epel(context);
    refresh_metadata(context);
}

fn needs_package_version_snapshot(entries: &[Entry], context: &Context<'_, impl Runner>) -> bool {
    // Batch package versions are useful only when command lookup alone cannot
    // prove every active package dependency. Avoiding the manager-wide query on
    // the common "all commands are on PATH" path keeps forced updates snappy
    // while still collapsing the expensive fallback for fonts/data packages and
    // package-name/command-name mismatches.
    entries.iter().any(|entry| {
        if entry.method != "pkg" || !active(entry, context.env) {
            return false;
        }
        let resolved = config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr));
        resolved != "NONE" && !context.runner.exists(&entry.cmd)
    })
}

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    queued: &mut Vec<Queued>,
    package_versions: &std::collections::BTreeMap<String, String>,
) -> Result<Item> {
    let resolved = config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr));
    if resolved == "NONE" {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "skipped by package-manager override".to_owned(),
        });
    }

    if process::dep_exists_with_versions(
        context.runner,
        &entry.cmd,
        &resolved,
        context.pkg_mgr,
        package_versions,
    ) {
        manifest::upsert(
            context.manifest_path,
            ManifestEntry::new(&entry.name, "pkg", &entry.cmd, ""),
        )?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: missing_command_needs_repair(entry, context),
            failed: false,
            detail: "installed".to_owned(),
        });
    }

    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(&entry.name, "pkg", &entry.cmd, ""),
    )?;

    if !available(context.runner, context.pkg_mgr, &resolved)? {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "not available".to_owned(),
        });
    }

    queued.push(Queued {
        name: entry.name.clone(),
        package: resolved,
    });
    Ok(Item {
        name: entry.name.clone(),
        changed: false,
        failed: false,
        detail: "queued".to_owned(),
    })
}

pub(crate) fn flush(
    queued: &[Queued],
    context: &Context<'_, impl Runner>,
    changed: &mut Vec<String>,
    summary: &mut Summary,
) -> Result<()> {
    if queued.is_empty() {
        return Ok(());
    }

    let packages = queued
        .iter()
        .map(|item| item.package.clone())
        .collect::<Vec<_>>();
    let Some(command) = pkg::install(context.pkg_mgr, &packages) else {
        return Ok(());
    };

    if run(context.runner, &command)?.success {
        changed.extend(queued.iter().map(|item| item.name.clone()));
        return Ok(());
    }

    // Bash retries package installs one-at-a-time after a failed batch so a
    // single bad package does not block every other queued dependency. Keep the
    // same failure isolation here even though it costs extra subprocesses only
    // on the uncommon failure path.
    for item in queued {
        let single = vec![item.package.clone()];
        let Some(command) = pkg::install(context.pkg_mgr, &single) else {
            continue;
        };
        if run(context.runner, &command)?.success {
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
) -> bool {
    if context.pkg_mgr.is_empty() {
        return false;
    }

    entries.iter().any(|entry| {
        if entry.method != "pkg" || !active(entry, context.env) {
            return false;
        }

        let resolved = config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr));
        resolved != "NONE"
            && !process::dep_exists_with_versions(
                context.runner,
                &entry.cmd,
                &resolved,
                context.pkg_mgr,
                package_versions,
            )
    })
}

fn maybe_enable_epel(context: &Context<'_, impl Runner>) {
    if context.pkg_mgr != "dnf"
        || context.env_vars.get("SHDEPS_AUTO_EPEL").map(String::as_str) != Some("1")
    {
        return;
    }

    // CentOS/RHEL-family machines often need EPEL for everyday CLI packages
    // such as ripgrep, and EPEL itself may depend on CodeReady Builder/CRB
    // being enabled first. This is intentionally best-effort like the Bash
    // implementation: if the host has no CRB, no EPEL package, or a locked-down
    // sudo policy, the per-dependency availability probe below still makes the
    // final skip/install decision without turning the whole update into a hard
    // failure.
    repair_dnf_optional_repo(context);

    if best_effort_run_raw(context.runner, "rpm", &["-q", "epel-release"])
        .is_some_and(|output| output.success)
    {
        return;
    }

    let packages = vec!["epel-release".to_owned()];
    let Some(command) = pkg::install("dnf", &packages) else {
        return;
    };
    let _ = best_effort_run(context.runner, &command);
}

fn repair_dnf_optional_repo(context: &Context<'_, impl Runner>) {
    for repo in ["crb", "powertools"] {
        let Some(output) = best_effort_run_raw(
            context.runner,
            "sudo",
            &["dnf", "config-manager", "--set-enabled", repo],
        ) else {
            continue;
        };
        if output.success {
            break;
        }
    }
}

fn refresh_metadata(context: &Context<'_, impl Runner>) {
    let Some(command) = pkg::refresh(context.pkg_mgr) else {
        return;
    };

    // Availability checks are only as good as the local package metadata. This
    // matters most in ephemeral CI containers: `apk add --no-cache cargo` can
    // install bootstrap tools without leaving an index for the later
    // `apk search -e jq` probe, and dnf needs an EPEL/metadata pass before
    // CentOS-only packages like ripgrep are visible. Refresh failures stay
    // nonfatal for Bash compatibility; the later availability probe still owns
    // the skip-vs-install decision for each dependency.
    let _ = best_effort_run(context.runner, &command);
}

fn available(runner: &impl Runner, mgr: &str, package: &str) -> Result<bool> {
    let Some(command) = pkg::available(mgr, package) else {
        return Ok(true);
    };
    let output = run(runner, &command)?;
    Ok(pkg::available_ok(mgr, output.success, &output.stdout))
}

fn run(runner: &impl Runner, command: &pkg::CommandSpec) -> Result<Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    run_raw(runner, &command.program, &args)
}

fn run_raw(runner: &impl Runner, program: &str, args: &[&str]) -> Result<Output> {
    runner.run(program, args, None).map_err(Into::into)
}

fn best_effort_run(runner: &impl Runner, command: &pkg::CommandSpec) -> Option<Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    best_effort_run_raw(runner, &command.program, &args)
}

fn best_effort_run_raw(runner: &impl Runner, program: &str, args: &[&str]) -> Option<Output> {
    // Package metadata preparation has always been advisory: it improves the
    // accuracy of availability probes, but a missing sudo/config-manager or a
    // transient repo refresh error must not hide the dependency-level result.
    // Install and availability calls still use `run_raw` so real install
    // failures are not silently swallowed.
    runner.run(program, args, None).ok()
}
