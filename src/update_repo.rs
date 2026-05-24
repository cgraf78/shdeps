//! `github:repo` update execution.
//!
//! Start with the local-dev-clone strategy because it is deterministic, fast,
//! and important for this repo's own workflow: a clone under `SHDEPS_GIT_DEV_DIR`
//! wins over network clone/pull and is exposed through a symlink in the managed
//! install directory.

use std::fs;

use crate::bin_link;
use crate::config::{self, Entry};
use crate::manifest::{self, ManifestEntry};
use crate::process::Runner;
use crate::update::{Context, Item, Options};
use crate::Result;

pub(crate) fn install(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Item> {
    // Local development clones deliberately win before any network work. This
    // keeps shdeps useful while hacking on cgraf78 repos: a fleet machine can
    // point at the checked-out repo under `~/git`, and update becomes a cheap
    // relink instead of a clone/pull against GitHub.
    let short = config::short_name(&entry.name);
    let local_clone = context.roots.git_dev_dir.join(short);
    if !local_clone.is_dir() {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: "github:repo network update is not implemented yet".to_owned(),
        });
    }

    let install_dir = context.roots.install_dir.join(&entry.name);
    let previous_target = fs::read_link(&install_dir).ok();
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    // `install_dir` is shdeps-owned for repo installs, even when it is only a
    // symlink to a development checkout. Replacing stale managed directories
    // here lets method transitions converge on the same canonical path without
    // ever deleting the real clone under `SHDEPS_GIT_DEV_DIR`.
    replace_symlink(&local_clone, &install_dir)?;

    bin_link::from_dir(
        &context.roots.state_dir,
        &context.roots.bin_dir,
        &entry.name,
        &install_dir,
    )?;
    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(
            &entry.name,
            "github:repo",
            &entry.cmd,
            install_dir.display().to_string(),
        ),
    )?;

    Ok(Item {
        name: entry.name.clone(),
        changed: options.reinstall || previous_target.as_ref() != Some(&local_clone),
        failed: false,
        detail: "local clone".to_owned(),
    })
}

#[cfg(unix)]
fn replace_symlink(target: &std::path::Path, link: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(link)?,
        Ok(_) => fs::remove_file(link)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    symlink(target, link)?;
    Ok(())
}
