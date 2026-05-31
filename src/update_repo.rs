//! `github:repo` update execution.
//!
//! Start with the local-dev-clone strategy because it is deterministic, fast,
//! and important for this repo's own workflow: a clone under `SHDEPS_GIT_DEV_DIR`
//! wins over network clone/pull and is exposed through a symlink in the managed
//! install directory.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::bin_link;
use crate::config::Entry;
use crate::extras;
use crate::manifest::{self, ManifestEntry};
use crate::method;
use crate::process::Runner;
use crate::repo;
use crate::stamp;
use crate::update::{Context, Item, ItemReason, Options, detail_with_action, verbose_enabled};

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

    let changed = options.reinstall
        || status.dirty
        || previous_target.as_ref() != Some(&local_clone)
        || rev_before != rev_after;
    let action = if previous_target.as_ref() != Some(&local_clone) {
        Some("added")
    } else if rev_before != rev_after {
        Some("updated")
    } else if options.reinstall || status.dirty {
        Some("reinstalled")
    } else {
        None
    };
    let mut detail = verbose_repo_detail(action, &local_clone, context, options, "local clone");
    if verbose_enabled(options, context.env_vars) && detail != "local clone" {
        detail = format!("{detail} (local clone)");
    }

    Ok(if changed {
        Item::changed(entry.name.clone(), ItemReason::Installed, detail)
    } else {
        Item::current(entry.name.clone(), ItemReason::Installed, detail)
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
        secure_managed_clone_permissions(install_dir)?;
        record_success(entry, context, install_dir)?;
        let detail = verbose_repo_detail(None, install_dir, context, options, "fresh");
        return Ok(Item::current(entry.name.clone(), ItemReason::Fresh, detail));
    }

    let head_before = git_head(context.runner, install_dir);
    let pulled = pull(context.runner, install_dir)
        || (prefer_ssh_origin(context.runner, install_dir) && pull(context.runner, install_dir));
    if !pulled {
        // Bash treats an existing clone pull failure as a warning, not an
        // install failure: the previous checkout is still usable, and hooks
        // should not run because no successful change happened.
        //
        // The pre-fix code reported every pull failure as the opaque string
        // `"update failed"`, which gave operators no way to distinguish a
        // transient network outage from a managed clone that diverged
        // because someone edited files inside it. Run `git status` to
        // bucket the failure: a dirty working tree is a user-recoverable
        // situation, anything else is most likely a network/fast-forward
        // problem that the next run will retry. Keep `failed: false` so a
        // shdeps update does not turn a transient network failure into a
        // hard build break — the more descriptive detail string is the
        // operator-visible signal.
        let post_status = git_status(context.runner, install_dir);
        // Three-way bucketing: a dirty working tree is the
        // user-recoverable case; a confirmed-clean tree with a pull
        // failure points at a network/no-fast-forward issue; an
        // unreported status (git command itself failed) means we
        // genuinely cannot classify and must say so rather than
        // guess. Lumping unreported into "no fast-forward" hid
        // broken-index/missing-git failures behind a misleading
        // label.
        let detail = if !post_status.reported {
            "pull failed (status unavailable)".to_owned()
        } else if post_status.dirty {
            "pull failed (dirty working tree)".to_owned()
        } else {
            "pull failed (no fast-forward)".to_owned()
        };
        secure_managed_clone_permissions(install_dir)?;
        record_success(entry, context, install_dir)?;
        return Ok(Item::current(entry.name.clone(), ItemReason::Other, detail));
    }

    let head_after = git_head(context.runner, install_dir);
    stamp::remote_touch(&stamp_path, options.now)?;
    secure_managed_clone_permissions(install_dir)?;
    record_success(entry, context, install_dir)?;
    let changed = options.reinstall || head_before != head_after;
    let action = if head_before != head_after {
        Some("updated")
    } else if options.reinstall {
        Some("reinstalled")
    } else {
        None
    };
    let detail = verbose_repo_detail(action, install_dir, context, options, "updated");
    Ok(if changed {
        Item::changed(entry.name.clone(), ItemReason::Installed, detail)
    } else {
        Item::current(entry.name.clone(), ItemReason::Installed, detail)
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
        return Ok(Item::failed(
            entry.name.clone(),
            ItemReason::MissingTool,
            "git not available",
        ));
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
        return Ok(Item::failed(
            entry.name.clone(),
            ItemReason::InstallFailed,
            "clone failed",
        ));
    }

    remove_any(install_dir)?;
    fs::rename(&clone_tmp, install_dir)?;
    set_ssh_push_url(context.runner, install_dir, url);
    secure_managed_clone_permissions(install_dir)?;
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    stamp::remote_touch(&stamp_path, options.now)?;
    record_success(entry, context, install_dir)?;

    let detail = verbose_repo_detail(Some("added"), install_dir, context, options, "added");
    Ok(Item::changed(
        entry.name.clone(),
        ItemReason::Installed,
        detail,
    ))
}

fn verbose_repo_detail(
    action: Option<&str>,
    install_dir: &Path,
    context: &Context<'_, impl Runner>,
    options: Options,
    fallback: &str,
) -> String {
    if !verbose_enabled(options, context.env_vars) {
        return fallback.to_owned();
    }

    let version = repo::version(install_dir, context.runner);
    match action {
        Some(action) => detail_with_action(action, version.unwrap_or_default()),
        None => version.unwrap_or_else(|| fallback.to_owned()),
    }
}

fn secure_managed_clone_permissions(install_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut pending = vec![install_dir.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }

            // Repo installs are often consumed directly by shells, not only
            // through shdeps' generated completion symlinks. Zsh's compaudit
            // rejects group/other-writable fpath directories and completion
            // files, so a permissive umask can turn an otherwise valid managed
            // clone into an interactive-shell prompt on every startup. Strip
            // only write bits from shdeps-owned managed clone paths; the
            // local-dev-clone path intentionally does not call this helper, so
            // real checkouts under `SHDEPS_GIT_DEV_DIR` keep user-selected
            // collaboration modes.
            if file_type.is_dir() || file_type.is_file() {
                let mode = metadata.permissions().mode();
                let secure_mode = mode & !0o022;
                if secure_mode != mode {
                    fs::set_permissions(&path, fs::Permissions::from_mode(secure_mode))?;
                }
            }

            if file_type.is_dir() {
                for entry in fs::read_dir(&path)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_symlink() {
                        pending.push(entry.path());
                    }
                }
            }
        }
    }

    Ok(())
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
            method::GITHUB_REPO,
            &entry.cmd,
            install_dir.display().to_string(),
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GitStatus {
    dirty: bool,
    /// Whether `git status` itself reported successfully. `false` means
    /// the command failed (broken git index, missing git binary, non-git
    /// directory, etc.) so `dirty` is just the default and callers
    /// should not treat the value as authoritative.
    reported: bool,
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
    // edge case because local clone detection is intentionally just `-d` — but
    // also record whether the command actually reported so callers can tell
    // "clean tree" apart from "couldn't ask".
    match output {
        Some(output) => GitStatus {
            dirty: !output.stdout.is_empty(),
            reported: true,
        },
        None => GitStatus {
            dirty: false,
            reported: false,
        },
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
