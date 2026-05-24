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
use crate::update::{Context, Item, Summary};
use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Queued {
    name: String,
    package: String,
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

fn available(runner: &impl Runner, mgr: &str, package: &str) -> Result<bool> {
    let Some(command) = pkg::available(mgr, package) else {
        return Ok(true);
    };
    let output = run(runner, &command)?;
    Ok(pkg::available_ok(mgr, output.success, &output.stdout))
}

fn run(runner: &impl Runner, command: &pkg::CommandSpec) -> Result<Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    runner
        .run(&command.program, &args, None)
        .map_err(Into::into)
}
