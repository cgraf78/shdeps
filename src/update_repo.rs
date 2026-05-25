//! `github:repo` update execution.
//!
//! Start with the local-dev-clone strategy because it is deterministic, fast,
//! and important for this repo's own workflow: a clone under `SHDEPS_GIT_DEV_DIR`
//! wins over network clone/pull and is exposed through a symlink in the managed
//! install directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::bin_link;
use crate::config::Entry;
use crate::extras;
use crate::manifest::{self, ManifestEntry};
use crate::process::Runner;
use crate::repo;
use crate::stamp;
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
    let source = repo::source(&entry.name, context.env_vars);
    let local_clone = context.roots.git_dev_dir.join(&source.short);
    if !local_clone.is_dir() {
        let install_dir = context.roots.install_dir.join(&source.name);
        return if install_dir.join(".git").is_dir() {
            install_existing(entry, context, options, &install_dir)
        } else {
            install_fresh(entry, context, options, &install_dir, &source.url)
        };
    }

    let install_dir = context.roots.install_dir.join(&entry.name);
    let previous_target = fs::read_link(&install_dir).ok();
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    let revision_path = stamp::revision_path(&context.roots.state_dir, &entry.name);
    let rev_before = stamp::revision_read(&revision_path)?;
    let mut status = git_status(context.runner, &local_clone);

    if !stamp::remote_fresh(&stamp_path, options.freshness()) && status.is_clean() {
        if has_upstream(context.runner, &local_clone) {
            if pull(context.runner, &local_clone) {
                stamp::remote_touch(&stamp_path, options.now)?;
            }
        } else {
            // Local-only dev clones are a valid dependency source. Touching the
            // stamp avoids repeating an upstream probe on every warm update.
            stamp::remote_touch(&stamp_path, options.now)?;
        }
        status = git_status(context.runner, &local_clone);
    }

    let rev_after = git_head(context.runner, &local_clone);
    if let Some(revision) = &rev_after {
        stamp::revision_touch(&revision_path, revision)?;
    }

    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    // `install_dir` is shdeps-owned for repo installs, even when it is only a
    // symlink to a development checkout. Replacing stale managed directories
    // here lets method transitions converge on the same canonical path without
    // ever deleting the real clone under `SHDEPS_GIT_DEV_DIR`.
    replace_symlink(&local_clone, &install_dir)?;

    record_success(entry, context, &install_dir)?;

    Ok(Item {
        name: entry.name.clone(),
        changed: options.reinstall
            || status.dirty
            || previous_target.as_ref() != Some(&local_clone)
            || rev_before != rev_after,
        failed: false,
        detail: "local clone".to_owned(),
    })
}

fn install_existing(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    install_dir: &Path,
) -> Result<Item> {
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    sync_ssh_push_url(context.runner, install_dir);

    if stamp::remote_fresh(&stamp_path, options.freshness()) {
        record_success(entry, context, install_dir)?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "fresh".to_owned(),
        });
    }

    let head_before = git_head(context.runner, install_dir);
    let pulled = pull(context.runner, install_dir)
        || (prefer_ssh_origin(context.runner, install_dir) && pull(context.runner, install_dir));
    if !pulled {
        // Bash treats an existing clone pull failure as a warning, not an
        // install failure: the previous checkout is still usable, and hooks
        // should not run because no successful change happened.
        record_success(entry, context, install_dir)?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: false,
            detail: "update failed".to_owned(),
        });
    }

    let head_after = git_head(context.runner, install_dir);
    stamp::remote_touch(&stamp_path, options.now)?;
    record_success(entry, context, install_dir)?;
    Ok(Item {
        name: entry.name.clone(),
        changed: options.reinstall || head_before != head_after,
        failed: false,
        detail: "updated".to_owned(),
    })
}

fn install_fresh(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    install_dir: &Path,
    url: &str,
) -> Result<Item> {
    if !context.runner.exists("git") {
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: "git not available".to_owned(),
        });
    }

    let clone_tmp = temp_clone_path(install_dir);
    remove_any(&clone_tmp)?;
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }

    let cloned = clone_repo(context.runner, url, &clone_tmp)
        || repo::ssh_fallback(url)
            .as_deref()
            .is_some_and(|fallback| clone_repo(context.runner, fallback, &clone_tmp));
    if !cloned || !clone_tmp.is_dir() {
        remove_any(&clone_tmp)?;
        return Ok(Item {
            name: entry.name.clone(),
            changed: false,
            failed: true,
            detail: "clone failed".to_owned(),
        });
    }

    remove_any(install_dir)?;
    fs::rename(&clone_tmp, install_dir)?;
    set_ssh_push_url(context.runner, install_dir, url);
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    stamp::remote_touch(&stamp_path, options.now)?;
    record_success(entry, context, install_dir)?;

    Ok(Item {
        name: entry.name.clone(),
        changed: true,
        failed: false,
        // Bash reported a newly materialized repo dependency as "added".
        // Downstream bootstrap tests and log triage treat that word as the
        // public install contract, while "cloned" is only the implementation
        // mechanism. Keep the user-facing status stable so a Rust binary can
        // replace the shell CLI without forcing client repos to relearn it.
        detail: "added".to_owned(),
    })
}

fn record_success(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    install_dir: &Path,
) -> Result<()> {
    bin_link::from_dir(
        &context.roots.state_dir,
        &context.roots.bin_dir,
        &entry.name,
        install_dir,
    )?;
    extras::link(
        &context.roots.state_dir,
        &context.roots.install_dir,
        &entry.name,
        install_dir,
    )?;
    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(
            &entry.name,
            "github:repo",
            &entry.cmd,
            install_dir.display().to_string(),
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GitStatus {
    dirty: bool,
}

impl GitStatus {
    fn is_clean(self) -> bool {
        !self.dirty
    }
}

fn git_status(runner: &impl Runner, dir: &Path) -> GitStatus {
    let output = git(
        runner,
        dir,
        &["status", "--porcelain", "--untracked-files=normal"],
    );
    // Bash captures `git status ... || true`, so a non-git directory or broken
    // git command behaves like an empty status string. Preserve that permissive
    // edge case because local clone detection is intentionally just `-d`.
    GitStatus {
        dirty: output.is_some_and(|output| !output.stdout.is_empty()),
    }
}

fn has_upstream(runner: &impl Runner, dir: &Path) -> bool {
    git(
        runner,
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .is_some()
}

fn git_head(runner: &impl Runner, dir: &Path) -> Option<String> {
    git(runner, dir, &["rev-parse", "HEAD"]).map(|output| output.stdout.trim().to_owned())
}

fn pull(runner: &impl Runner, dir: &Path) -> bool {
    git(runner, dir, &["pull", "--ff-only", "--quiet"]).is_some()
}

fn remote_origin(runner: &impl Runner, dir: &Path) -> Option<String> {
    git(runner, dir, &["remote", "get-url", "origin"]).map(|output| output.stdout.trim().to_owned())
}

fn prefer_ssh_origin(runner: &impl Runner, install_dir: &Path) -> bool {
    let Some(origin) = remote_origin(runner, install_dir) else {
        return false;
    };
    let Some(fallback) = repo::ssh_fallback(&origin) else {
        return false;
    };
    if git(
        runner,
        install_dir,
        &["remote", "set-url", "origin", &fallback],
    )
    .is_none()
    {
        return false;
    }
    set_push_url(runner, install_dir, &fallback);
    true
}

fn sync_ssh_push_url(runner: &impl Runner, install_dir: &Path) {
    let Some(origin) = remote_origin(runner, install_dir) else {
        return;
    };
    let fallback = if origin.starts_with("git@github.com:") {
        Some(origin)
    } else {
        repo::ssh_fallback(&origin)
    };
    if let Some(fallback) = fallback {
        set_push_url(runner, install_dir, &fallback);
    }
}

fn set_ssh_push_url(runner: &impl Runner, install_dir: &Path, url: &str) {
    if let Some(fallback) = repo::ssh_fallback(url) {
        set_push_url(runner, install_dir, &fallback);
    }
}

fn set_push_url(runner: &impl Runner, install_dir: &Path, url: &str) {
    let _ = git(
        runner,
        install_dir,
        &["remote", "set-url", "--push", "origin", url],
    );
}

fn clone_repo(runner: &impl Runner, url: &str, target: &Path) -> bool {
    let target = target.display().to_string();
    runner
        .run("git", &["clone", "--depth", "1", url, &target], None)
        .ok()
        .is_some_and(|output| output.success)
}

fn git(runner: &impl Runner, dir: &Path, args: &[&str]) -> Option<crate::process::Output> {
    let dir = dir.display().to_string();
    let mut full = Vec::with_capacity(args.len() + 2);
    full.push("-C");
    full.push(dir.as_str());
    full.extend_from_slice(args);
    runner
        .run("git", &full, None)
        .ok()
        .filter(|output| output.success)
}

fn temp_clone_path(install_dir: &Path) -> PathBuf {
    let mut tmp = install_dir.to_path_buf();
    let name = install_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    tmp.set_file_name(format!("{name}.tmp.{}", std::process::id()));
    tmp
}

fn remove_any(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
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
