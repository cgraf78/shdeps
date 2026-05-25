//! Package phase for `shdeps update`.
//!
//! The package phase has enough policy to deserve its own module: package deps
//! run before every other method, unavailable packages are compatibility skips,
//! and batch failures retry one package at a time. Keeping that here prevents
//! the top-level update orchestrator from becoming another monolith.

use crate::config::{self, Entry};
use crate::manifest::{self, ManifestEntry};
use crate::pkg;
use crate::process::{self, Output, Runner};
use crate::update::{active, Context, Item, Summary};
use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Queued {
    pub(crate) name: String,
    package: String,
}

pub(crate) fn prepare(entries: &[Entry], context: &Context<'_, impl Runner>) {
    if !needs_package_work(entries, context) {
        return;
    }

    maybe_enable_epel(context);
    refresh_metadata(context);
}

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    queued: &mut Vec<Queued>,
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

    if process::dep_exists(context.runner, &entry.cmd, &resolved, context.pkg_mgr) {
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

fn needs_package_work(entries: &[Entry], context: &Context<'_, impl Runner>) -> bool {
    if context.pkg_mgr.is_empty() {
        return false;
    }

    entries.iter().any(|entry| {
        if entry.method != "pkg" || !active(entry, context.env) {
            return false;
        }

        let resolved = config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr));
        resolved != "NONE"
            && !process::dep_exists(context.runner, &entry.cmd, &resolved, context.pkg_mgr)
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
