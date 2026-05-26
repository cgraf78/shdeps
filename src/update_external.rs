//! External toolchain phase for `shdeps update`.
//!
//! This module owns the shared `cargo`/`go`/`uv`/`npm` install skeleton. The
//! method-specific command construction lives in `external`; this layer handles
//! the runtime contract that all four methods share: TTL fast path, required
//! tool detection, expected binary validation, public symlink repair, stamp
//! update, and manifest write.

use std::fs;

use crate::Result;
use crate::bin_link;
use crate::config::Entry;
use crate::external;
use crate::manifest::{self, ManifestEntry};
use crate::pkg::CommandSpec;
use crate::process::{self, Output, Runner};
use crate::stamp;
use crate::update::{Context, Item, Options};

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Item> {
    let Some(plan) = external::plan(
        &entry.method,
        &entry.name,
        &entry.cmd,
        &context.roots.install_dir,
    ) else {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: "unsupported external method".to_owned(),
        });
    };

    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, &entry.method);
    if process::executable_path(&plan.bin_path)
        && stamp::remote_fresh(&stamp_path, options.freshness())
    {
        bin_link::one(&context.roots.bin_dir, &entry.cmd, &plan.bin_path)?;
        write_manifest(entry, context, &plan)?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "fresh".to_owned(),
        });
    }

    if !context.runner.exists(&plan.tool) {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: format!("{} not found", plan.tool),
        });
    }

    let already_installed = process::executable_path(&plan.bin_path);
    let previous_version = already_installed
        .then(|| process::dep_version(context.runner, &entry.cmd))
        .flatten();

    fs::create_dir_all(plan.install_root.join("bin"))?;
    let command = plan.command_for(options.reinstall);
    if !run(context.runner, &command)?.success {
        // Best-effort cleanup: if the install never produced any
        // content under the freshly-created install_root, remove the
        // empty `bin/` and `install_root` so a failed first-time
        // install does not leave stub directories scattered under the
        // managed tree. `remove_dir` fails on non-empty directories,
        // which is exactly what we want — a reinstall that failed
        // mid-stream still leaves the dep's previous state intact.
        let _ = fs::remove_dir(plan.install_root.join("bin"));
        let _ = fs::remove_dir(&plan.install_root);
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: format!("{} install failed", plan.tool),
        });
    }

    if !process::executable_path(&plan.bin_path) {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: "install produced no binary".to_owned(),
        });
    }

    bin_link::one(&context.roots.bin_dir, &entry.cmd, &plan.bin_path)?;
    stamp::remote_touch(&stamp_path, options.now)?;
    write_manifest(entry, context, &plan)?;

    let version = process::dep_version(context.runner, &entry.cmd);
    let changed = !already_installed
        || options.reinstall
        || previous_version
            .as_ref()
            .zip(version.as_ref())
            .is_some_and(|(before, after)| before != after);

    Ok(Item {
        name: entry.name.clone(),
        changed,
        failed: false,
        detail: version.unwrap_or_else(|| "installed".to_owned()),
    })
}

fn write_manifest(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    plan: &external::Plan,
) -> Result<()> {
    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(
            &entry.name,
            &entry.method,
            &entry.cmd,
            plan.bin_path.display().to_string(),
        ),
    )
}

fn run(runner: &impl Runner, command: &CommandSpec) -> Result<Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    runner
        .run(&command.program, &args, None)
        .map_err(Into::into)
}
