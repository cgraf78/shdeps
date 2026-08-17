//! `github:repo` update execution.
//!
//! Local development clones remain the preferred source for absent or already
//! managed destinations. An unrecorded ordinary destination is different: it
//! is preserved and independently verified before a development clone may
//! replace anything at the canonical managed path.

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
use crate::repo_adopt;
use crate::repo_verify;
use crate::stamp;
use crate::update::{Context, Item, ItemReason, Options, detail_with_action, verbose_enabled};

/// Authority for a preexisting canonical repository destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestinationOwnership {
    /// The current manifest already records this dependency as `github:repo`.
    RecordedRepo,
    /// A validated previous built-in method owns the same canonical root.
    PreviousMethod,
    /// No Shdeps state authorizes mutation; adoption must prove the root first.
    Unrecorded,
}

/// Result of the inert repository preparation phase.
pub(crate) enum Preparation {
    /// All proof succeeded and the mutating phase may consume this plan.
    Ready(Box<InstallPlan>),
    /// A normal user-facing compatibility failure occurred before mutation.
    Failed(Item),
}

/// Opaque plan binding source selection and any adoption capability to one run.
pub(crate) struct InstallPlan {
    source: repo::Source,
    local_clone: PathBuf,
    install_dir: PathBuf,
    route: InstallRoute,
}

enum InstallRoute {
    Managed,
    Fresh,
    Development {
        verified: repo_verify::VerifiedDevelopment,
        replace_owned_destination: bool,
    },
    Adopted(repo_verify::VerifiedOrdinary),
}

/// Inspects and verifies a repo install without changing transition or live
/// installation state. Callers must run this before transition preparation.
pub(crate) fn prepare(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    install_dir: &Path,
    ownership: DestinationOwnership,
) -> Result<Preparation> {
    // Resolve the development source before any network work. It wins for an
    // absent or already managed destination, but it never grants permission to
    // replace an unrecorded ordinary checkout: that root must be preserved and
    // independently adopted first.
    let source = repo::source(&entry.name, context.env_vars);
    let local_clone = context.roots.git_dev_dir.join(&source.short);
    let origin_policy = repo::OriginPolicy::new(&source.url);
    let route = if ownership != DestinationOwnership::Unrecorded {
        if local_clone.is_dir() {
            match development_route(entry, context, &local_clone, &source.url, true) {
                Ok(route) => route,
                Err(item) => return Ok(Preparation::Failed(item)),
            }
        } else {
            InstallRoute::Managed
        }
    } else {
        let destination =
            match repo_adopt::inspect_destination(install_dir, &local_clone, &origin_policy) {
                Ok(destination) => destination,
                Err(error) => return Ok(Preparation::Failed(adoption_failure(entry, error))),
            };
        match destination {
            repo_adopt::Destination::Absent if local_clone.is_dir() => {
                match development_route(entry, context, &local_clone, &source.url, false) {
                    Ok(route) => route,
                    Err(item) => return Ok(Preparation::Failed(item)),
                }
            }
            repo_adopt::Destination::Absent => InstallRoute::Fresh,
            repo_adopt::Destination::DevelopmentLink => {
                match development_route(entry, context, &local_clone, &source.url, false) {
                    Ok(route) => route,
                    Err(item) => return Ok(Preparation::Failed(item)),
                }
            }
            repo_adopt::Destination::Ordinary(candidate) => {
                let verification = match verify_ordinary_with_fallback(
                    &candidate,
                    install_dir,
                    &context.roots.state_dir,
                    &source.url,
                    entry,
                    context,
                ) {
                    Ok(verification) => verification,
                    Err(error) => {
                        return Ok(Preparation::Failed(adoption_failure(entry, error)));
                    }
                };
                match verification {
                    repo_verify::Verification::Verified(verified) => {
                        InstallRoute::Adopted(verified)
                    }
                    repo_verify::Verification::MissingCommand => {
                        return Ok(Preparation::Failed(missing_command_item(entry)));
                    }
                }
            }
        }
    };

    Ok(Preparation::Ready(Box::new(InstallPlan {
        source,
        local_clone,
        install_dir: install_dir.to_path_buf(),
        route,
    })))
}

fn development_route(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    local_clone: &Path,
    configured_origin: &str,
    replace_owned_destination: bool,
) -> std::result::Result<InstallRoute, Item> {
    let request = repo_verify::DevelopmentRequest {
        root: local_clone,
        configured_origin,
        command: &entry.cmd,
        command_explicit: entry.cmd_explicit,
        env_vars: context.env_vars,
    };
    match repo_verify::verify_development(&request, context.runner) {
        Ok(repo_verify::DevelopmentVerification::Verified(verified)) => {
            Ok(InstallRoute::Development {
                verified,
                replace_owned_destination,
            })
        }
        Ok(repo_verify::DevelopmentVerification::MissingCommand) => {
            Err(missing_command_item(entry))
        }
        Err(error) => Err(development_failure(entry, error)),
    }
}

fn verify_ordinary_with_fallback(
    candidate: &repo_adopt::OrdinaryCandidate,
    install_dir: &Path,
    state_dir: &Path,
    origin: &str,
    entry: &Entry,
    context: &Context<'_, impl Runner>,
) -> Result<repo_verify::Verification> {
    let request = repo_verify::OrdinaryRequest {
        root: install_dir,
        state_dir,
        approved_origin: origin,
        command: &entry.cmd,
        command_explicit: entry.cmd_explicit,
        env_vars: context.env_vars,
        trusted_home: &context.roots.home,
    };
    match repo_verify::verify_ordinary(candidate, &request, context.runner) {
        Ok(verification) => Ok(verification),
        Err(primary) if primary.allows_ssh_fallback() => {
            let Some(fallback) = repo::ssh_fallback(origin) else {
                return Err(std::io::Error::other(primary.to_string()).into());
            };
            let fallback_request = repo_verify::OrdinaryRequest {
                approved_origin: &fallback,
                ..request
            };
            repo_verify::verify_ordinary(candidate, &fallback_request, context.runner).map_err(
                |secondary| {
                    std::io::Error::other(format!(
                        "HTTPS verification failed: {primary}; SSH fallback failed: {secondary}"
                    ))
                    .into()
                },
            )
        }
        Err(error) => Err(std::io::Error::other(error.to_string()).into()),
    }
}

/// Applies one already-prepared plan while the shared checkout lock is held.
pub(crate) fn apply(
    plan: InstallPlan,
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Item> {
    match plan.route {
        InstallRoute::Managed if plan.install_dir.join(".git").is_dir() => {
            install_existing(entry, context, options, &plan.install_dir)
        }
        InstallRoute::Managed => install_fresh(
            entry,
            context,
            options,
            &plan.install_dir,
            &plan.source.url,
            true,
        ),
        InstallRoute::Fresh => {
            require_still_absent(&plan.install_dir)?;
            install_fresh(
                entry,
                context,
                options,
                &plan.install_dir,
                &plan.source.url,
                false,
            )
        }
        InstallRoute::Development {
            verified,
            replace_owned_destination,
        } => {
            require_development_destination(
                &plan.install_dir,
                &plan.local_clone,
                replace_owned_destination,
            )?;
            install_development(
                entry,
                context,
                options,
                &plan.local_clone,
                &plan.install_dir,
                verified,
                replace_owned_destination,
            )
        }
        InstallRoute::Adopted(verified) => {
            install_verified_existing(entry, context, options, &plan.install_dir, verified)
        }
    }
}

fn adoption_failure(entry: &Entry, error: impl std::fmt::Display) -> Item {
    Item::failed(
        entry.name.clone(),
        ItemReason::InstallFailed,
        format!("refusing to adopt existing checkout: {error}"),
    )
}

fn development_failure(entry: &Entry, error: impl std::fmt::Display) -> Item {
    Item::failed(
        entry.name.clone(),
        ItemReason::InstallFailed,
        format!("refusing development checkout: {error}"),
    )
}

// The absence proof is part of source selection. Rechecking it prevents an
// uncoordinated writer from turning a safe fresh plan into recursive deletion.
fn require_still_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository destination appeared after preparation",
        )
        .into()),
    }
}

// This is a post-lock race check, not ownership discovery. Unrecorded plans
// accept only absence or the exact prepared development link.
// `replace_owned_destination` is granted by structural manifest/transition
// evidence; filesystem-derived release evidence is additionally revalidated
// under the checkout lock. It lets `repo_transition` replace an owned directory
// or symlink while unsupported objects still fail closed.
fn require_development_destination(
    path: &Path,
    local_clone: &Path,
    replace_owned_destination: bool,
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(path) {
            Ok(target) if target == local_clone => Ok(()),
            Ok(_) if replace_owned_destination => Ok(()),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "repository destination changed after development-source preparation",
            )
            .into()),
            Err(error) => Err(error.into()),
        },
        Ok(_) if replace_owned_destination => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository destination changed after development-source preparation",
        )
        .into()),
    }
}

// Publish and refresh a deliberately selected local development checkout.
fn install_development(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    local_clone: &Path,
    install_dir: &Path,
    verified: repo_verify::VerifiedDevelopment,
    replace_owned_destination: bool,
) -> Result<Item> {
    verified.authorize(local_clone)?;

    let previous_target = fs::read_link(install_dir).ok();
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    let revision_path = stamp::revision_path(&context.roots.state_dir, &entry.name);
    let rev_before = stamp::revision_read(&revision_path)?;
    let mut status = development_git_status(&verified, context.runner, local_clone)?;
    let mut refresh_stamp = false;
    let mut pull_failed = false;

    if !stamp::remote_fresh(&stamp_path, options.freshness()) && status.is_clean() {
        if development_has_upstream(&verified, context.runner, local_clone)? {
            if development_pull(&verified, context.runner, local_clone)? {
                refresh_stamp = true;
            } else {
                // A local development clone is user-owned, so shdeps must not
                // reset or rebase it. Keep serving the checkout, but preserve
                // the failed pull as a first-class warning instead of making a
                // stale command look current.
                pull_failed = true;
            }
        } else {
            // Local-only dev clones are a valid dependency source. Touching the
            // stamp avoids repeating an upstream probe on every warm update.
            refresh_stamp = true;
        }
        status = development_git_status(&verified, context.runner, local_clone)?;
    }

    let rev_after = development_git_head(&verified, context.runner, local_clone)?;

    if !verified.revalidate(local_clone, context.runner)? {
        return Ok(missing_command_item(entry));
    }
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    publish_development_link(local_clone, install_dir, replace_owned_destination)?;
    if let Some(revision) = &rev_after {
        stamp::revision_touch(&revision_path, revision)?;
    }
    if refresh_stamp {
        stamp::remote_touch(&stamp_path, options.now)?;
    }
    record_success(entry, context, install_dir)?;

    let changed = options.reinstall
        || status.dirty
        || previous_target.as_deref() != Some(local_clone)
        || rev_before != rev_after;
    let action = if previous_target.as_deref() != Some(local_clone) {
        Some("added")
    } else if rev_before != rev_after {
        Some("updated")
    } else if options.reinstall || status.dirty {
        Some("reinstalled")
    } else {
        None
    };
    let mut detail = verbose_repo_detail(action, local_clone, context, options, "local clone");
    if verbose_enabled(options, context.env_vars) && detail != "local clone" {
        detail = format!("{detail} (local clone)");
    }

    Ok(if pull_failed {
        Item::warning(
            entry.name.clone(),
            ItemReason::RepoPullFailed,
            local_pull_failure_detail(status),
            changed,
        )
    } else if changed {
        Item::changed(entry.name.clone(), ItemReason::Installed, detail)
    } else {
        Item::current(entry.name.clone(), ItemReason::Installed, detail)
    })
}

// Adopt a checkout only after quarantine proved its exact root and contents.
// Unlike an already-recorded checkout, this path does not pull: the verifier
// independently established that the candidate equals the current remote
// default before granting the capability consumed here.
fn install_verified_existing(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    install_dir: &Path,
    verified: repo_verify::VerifiedOrdinary,
) -> Result<Item> {
    verified.authorize(install_dir)?;
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    let was_fresh = stamp::remote_fresh(&stamp_path, options.freshness());
    sync_ssh_push_url(context.runner, install_dir);
    secure_managed_clone_permissions(install_dir)?;
    if let Some(item) = missing_explicit_command(entry, install_dir) {
        return Ok(item);
    }
    if !was_fresh {
        stamp::remote_touch(&stamp_path, options.now)?;
    }
    record_success(entry, context, install_dir)?;

    if was_fresh {
        let detail = verbose_repo_detail(None, install_dir, context, options, "fresh");
        Ok(Item::current(entry.name.clone(), ItemReason::Fresh, detail))
    } else if options.reinstall {
        let detail = verbose_repo_detail(
            Some("reinstalled"),
            install_dir,
            context,
            options,
            "reinstalled",
        );
        Ok(Item::changed(
            entry.name.clone(),
            ItemReason::Installed,
            detail,
        ))
    } else {
        let detail = verbose_repo_detail(None, install_dir, context, options, "updated");
        Ok(Item::current(
            entry.name.clone(),
            ItemReason::Installed,
            detail,
        ))
    }
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
        if let Some(item) = missing_explicit_command(entry, install_dir) {
            return Ok(item);
        }
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
        let detail = pull_failure_detail(post_status);
        secure_managed_clone_permissions(install_dir)?;
        if let Some(item) = missing_explicit_command(entry, install_dir) {
            return Ok(item);
        }
        record_success(entry, context, install_dir)?;
        return Ok(Item::warning(
            entry.name.clone(),
            ItemReason::RepoPullFailed,
            detail,
            false,
        ));
    }

    let head_after = git_head(context.runner, install_dir);
    secure_managed_clone_permissions(install_dir)?;
    if let Some(item) = missing_explicit_command(entry, install_dir) {
        return Ok(item);
    }
    stamp::remote_touch(&stamp_path, options.now)?;
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
    replace_owned_destination: bool,
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

    secure_managed_clone_permissions(&clone_tmp)?;
    if let Some(item) = missing_explicit_command(entry, &clone_tmp) {
        remove_any(&clone_tmp)?;
        return Ok(item);
    }

    #[cfg(unix)]
    if let Err(error) = crate::repo_transition::publish_directory(
        install_dir,
        &clone_tmp,
        replace_owned_destination,
    ) {
        remove_any(&clone_tmp)?;
        return Err(error);
    }
    #[cfg(not(unix))]
    {
        if !replace_owned_destination {
            require_still_absent(install_dir)?;
        }
        remove_any(install_dir)?;
        fs::rename(&clone_tmp, install_dir)?;
    }
    set_ssh_push_url(context.runner, install_dir, url);
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

fn missing_explicit_command(entry: &Entry, install_dir: &Path) -> Option<Item> {
    if !repo::missing_explicit_command(entry, install_dir) {
        return None;
    }

    Some(missing_command_item(entry))
}

fn missing_command_item(entry: &Entry) -> Item {
    Item::failed(
        entry.name.clone(),
        ItemReason::MissingBinary,
        format!("configured command `{}` not found in repo bin", entry.cmd),
    )
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

fn development_git_status(
    verified: &repo_verify::VerifiedDevelopment,
    runner: &impl Runner,
    dir: &Path,
) -> Result<GitStatus> {
    let output = verified.run_git(
        dir,
        runner,
        &["status", "--porcelain", "--untracked-files=normal"],
    )?;
    if output.timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "development checkout status timed out",
        )
        .into());
    }
    if !output.success {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "development checkout status failed",
        )
        .into());
    }
    Ok(GitStatus {
        dirty: !output.stdout.is_empty(),
        reported: true,
    })
}

fn development_has_upstream(
    verified: &repo_verify::VerifiedDevelopment,
    runner: &impl Runner,
    dir: &Path,
) -> Result<bool> {
    let output = verified.run_git(
        dir,
        runner,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )?;
    if output.timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "development checkout upstream lookup timed out",
        )
        .into());
    }
    Ok(output.success)
}

fn development_pull(
    verified: &repo_verify::VerifiedDevelopment,
    runner: &impl Runner,
    dir: &Path,
) -> Result<bool> {
    Ok(verified.run_pull(dir, runner)?.success)
}

fn development_git_head(
    verified: &repo_verify::VerifiedDevelopment,
    runner: &impl Runner,
    dir: &Path,
) -> Result<Option<String>> {
    let output = verified.run_git(dir, runner, &["rev-parse", "HEAD"])?;
    if output.timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "development checkout HEAD lookup timed out",
        )
        .into());
    }
    Ok(output.success.then(|| output.stdout.trim().to_owned()))
}

fn pull_failure_detail(status: GitStatus) -> String {
    format!("pull failed ({})", pull_failure_cause(status))
}

fn local_pull_failure_detail(status: GitStatus) -> String {
    format!("pull failed ({}; local clone)", pull_failure_cause(status))
}

fn pull_failure_cause(status: GitStatus) -> &'static str {
    if !status.reported {
        "status unavailable"
    } else if status.dirty {
        "dirty working tree"
    } else {
        "no fast-forward"
    }
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
fn publish_development_link(
    target: &std::path::Path,
    link: &std::path::Path,
    replace_owned_destination: bool,
) -> Result<()> {
    crate::repo_transition::publish_development(link, target, replace_owned_destination)
}
