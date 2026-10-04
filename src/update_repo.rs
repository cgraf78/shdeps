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
use crate::cancellation;
use crate::config::Entry;
use crate::extras;
use crate::hooks::MutationIntent;
use crate::manifest::{self, ManifestEntry};
use crate::method;
use crate::process::Runner;
use crate::repo;
use crate::repo_adopt;
use crate::repo_verify;
use crate::stale_remote;
use crate::stamp;
use crate::state;
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
    Development(DevelopmentPlan),
    Adopted(repo_verify::VerifiedOrdinary),
}

struct DevelopmentPlan {
    verified: repo_verify::VerifiedDevelopment,
    replace_owned_destination: bool,
}

/// Inspects and verifies a repo install without changing transition or live
/// installation state. Callers must run this before transition preparation.
pub(crate) fn prepare(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    install_dir: &Path,
    ownership: DestinationOwnership,
) -> Result<Preparation> {
    cancellation::check()?;
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

    cancellation::check()?;
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
            Ok(InstallRoute::Development(DevelopmentPlan {
                verified,
                replace_owned_destination,
            }))
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
    mutation: &mut MutationIntent,
) -> Result<Item> {
    cancellation::check()?;
    match plan.route {
        InstallRoute::Managed if plan.install_dir.join(".git").is_dir() => install_existing(
            entry,
            context,
            options,
            &plan.install_dir,
            &plan.source.url,
            mutation,
        ),
        InstallRoute::Managed => install_fresh(
            entry,
            context,
            options,
            &plan.install_dir,
            &plan.source.url,
            true,
            mutation,
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
                mutation,
            )
        }
        InstallRoute::Development(development) => {
            require_development_destination(
                &plan.install_dir,
                &plan.local_clone,
                development.replace_owned_destination,
            )?;
            install_development(
                entry,
                context,
                options,
                &plan.local_clone,
                &plan.install_dir,
                development,
                mutation,
            )
        }
        InstallRoute::Adopted(verified) => install_verified_existing(
            entry,
            context,
            options,
            &plan.install_dir,
            verified,
            mutation,
        ),
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
    development: DevelopmentPlan,
    mutation: &mut MutationIntent,
) -> Result<Item> {
    let DevelopmentPlan {
        verified,
        replace_owned_destination,
    } = development;
    cancellation::check()?;
    verified.authorize(local_clone)?;

    let previous_target = fs::read_link(install_dir).ok();
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    let revision_path = stamp::revision_path(&context.roots.state_dir, &entry.name);
    let rev_before = stamp::revision_read(&revision_path)?;
    let mut status = development_git_status(&verified, context.runner, local_clone)?;
    let mut refresh_stamp = false;
    let mut pull_failure: Option<String> = None;

    if !stamp::remote_fresh(&stamp_path, options.freshness()) && status.is_clean() {
        if development_has_upstream(&verified, context.runner, local_clone)? {
            mutation.begin()?;
            match development_pull(&verified, context.runner, local_clone)? {
                Ok(()) => refresh_stamp = true,
                // A local development clone is user-owned, so shdeps must not
                // reset or rebase it. Keep serving the checkout, but preserve
                // the failed pull as a first-class warning instead of making a
                // stale command look current.
                Err(stderr) => pull_failure = Some(stderr),
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
    cancellation::check()?;
    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    mutation.begin()?;
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
    if pull_failure.is_none() {
        let _ = mutation.resolve(changed)?;
    }
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

    Ok(if let Some(stderr) = pull_failure {
        Item::warning(
            entry.name.clone(),
            ItemReason::RepoPullFailed,
            local_pull_failure_detail(status, &stderr),
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
    mutation: &mut MutationIntent,
) -> Result<Item> {
    cancellation::check()?;
    verified.authorize(install_dir)?;
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    let was_fresh = stamp::remote_fresh(&stamp_path, options.freshness());
    mutation.begin()?;
    let _push_url_changed = sync_ssh_push_url(context.runner, install_dir);
    cancellation::check()?;
    if was_fresh {
        secure_managed_clone_permissions_cached(
            &context.roots.state_dir,
            &entry.name,
            install_dir,
        )?;
    } else {
        secure_managed_clone_permissions(install_dir)?;
        let _ = write_permwalk_stamp(&context.roots.state_dir, &entry.name, install_dir);
    }
    if let Some(item) = missing_explicit_command(entry, install_dir) {
        return Ok(item);
    }
    if !was_fresh {
        stamp::remote_touch(&stamp_path, options.now)?;
        let _ = stale_remote::clear(&context.roots.state_dir, &entry.name);
    }
    record_success(entry, context, install_dir)?;

    // Adoption itself commits ownership, links, and a manifest row even when
    // the checkout contents were already current.
    let _ = mutation.resolve(true)?;

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
    configured_url: &str,
    mutation: &mut MutationIntent,
) -> Result<Item> {
    cancellation::check()?;
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    mutation.begin()?;
    let push_url_changed = sync_ssh_push_url(context.runner, install_dir);
    cancellation::check()?;

    if stamp::remote_fresh(&stamp_path, options.freshness()) {
        secure_managed_clone_permissions_cached(
            &context.roots.state_dir,
            &entry.name,
            install_dir,
        )?;
        if let Some(item) = missing_explicit_command(entry, install_dir) {
            return Ok(item);
        }
        record_success(entry, context, install_dir)?;
        let _ = mutation.resolve(push_url_changed)?;
        let detail = verbose_repo_detail(None, install_dir, context, options, "fresh");
        return Ok(Item::current(entry.name.clone(), ItemReason::Fresh, detail));
    }

    let head_before = git_head(context.runner, install_dir);
    let refreshed = refresh(context.runner, install_dir, configured_url);
    cancellation::check()?;
    if let Err(failure) = refreshed {
        // An existing clone that cannot be refreshed is a warning, not an
        // install failure: the previous checkout is still usable, a network
        // blip must not turn a cron update red, and hooks should not run
        // because nothing changed. The cause comes from Git itself, so a
        // transient fetch failure no longer reads like a diverged checkout.
        //
        // The record keeps that warning from being forgotten: it survives
        // until a refresh succeeds, so `shdeps health` can report a checkout
        // that has been stuck for a day (`stale-remote`). It is diagnostic,
        // so failing to write it must not fail the update.
        let _ = stale_remote::record_failure(
            &context.roots.state_dir,
            &entry.name,
            &failure,
            options.now,
        );
        secure_managed_clone_permissions(install_dir)?;
        let _ = write_permwalk_stamp(&context.roots.state_dir, &entry.name, install_dir);
        if let Some(item) = missing_explicit_command(entry, install_dir) {
            return Ok(item);
        }
        record_success(entry, context, install_dir)?;
        return Ok(Item::warning(
            entry.name.clone(),
            ItemReason::RepoPullFailed,
            format!("pull failed ({})", failure.cause()),
            false,
        ));
    }

    let head_after = git_head(context.runner, install_dir);
    cancellation::check()?;
    secure_managed_clone_permissions(install_dir)?;
    let _ = write_permwalk_stamp(&context.roots.state_dir, &entry.name, install_dir);
    if let Some(item) = missing_explicit_command(entry, install_dir) {
        return Ok(item);
    }
    stamp::remote_touch(&stamp_path, options.now)?;
    let _ = stale_remote::clear(&context.roots.state_dir, &entry.name);
    record_success(entry, context, install_dir)?;
    let changed = options.reinstall || head_before != head_after;
    let _ = mutation.resolve(changed || push_url_changed)?;
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
    mutation: &mut MutationIntent,
) -> Result<Item> {
    cancellation::check()?;
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
    if let Err(error) = cancellation::check() {
        remove_any(&clone_tmp)?;
        return Err(error.into());
    }
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

    mutation.begin()?;
    #[cfg(unix)]
    let publication = if replace_owned_destination {
        crate::repo_transition::publish_directory(install_dir, &clone_tmp, true)
    } else {
        crate::repo_transition::publish_fresh_directory(
            install_dir,
            &clone_tmp,
            ManifestEntry::new(
                &entry.name,
                method::GITHUB_REPO,
                &entry.cmd,
                install_dir.display().to_string(),
            ),
            &context.roots.state_dir,
        )
    };
    #[cfg(unix)]
    if let Err(error) = publication {
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
    cancellation::check()?;
    let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "repo");
    stamp::remote_touch_cancellable(&stamp_path, options.now)?;
    // A fresh clone supersedes whatever stopped the previous checkout.
    let _ = stale_remote::clear(&context.roots.state_dir, &entry.name);
    cancellation::check()?;
    record_success(entry, context, install_dir)?;
    #[cfg(unix)]
    if !replace_owned_destination {
        let ownership = ManifestEntry::new(
            &entry.name,
            method::GITHUB_REPO,
            &entry.cmd,
            install_dir.display().to_string(),
        );
        crate::hooks::mark_pending_post(&context.roots.state_dir, &entry.name)?;
        crate::repo_transition::finish_fresh_recovery(
            install_dir,
            &ownership,
            &context.roots.state_dir,
        )?;
    }

    let _ = mutation.resolve(true)?;

    let detail = verbose_repo_detail(Some("added"), install_dir, context, options, "added");
    Ok(Item::changed(
        entry.name.clone(),
        ItemReason::Installed,
        detail,
    ))
}

#[cfg(unix)]
pub(crate) fn recover_fresh_publication(
    roots: &crate::runtime::Roots,
    manifest_path: &Path,
    install_dir: &Path,
    ownership: &ManifestEntry,
) -> Result<()> {
    if ownership.method != method::GITHUB_REPO
        || !crate::config::valid_dep_name(&ownership.name)
        || !crate::config::valid_cmd_basename(&ownership.cmd)
        || Path::new(&ownership.install_path) != install_dir
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fresh repository recovery has inconsistent durable ownership",
        )
        .into());
    }
    let entry = Entry {
        name: ownership.name.clone(),
        method: method::GITHUB_REPO.to_owned(),
        cmd: ownership.cmd.clone(),
        cmd_explicit: true,
        aliases: String::new(),
        filter: String::new(),
    };
    record_success_with_roots(&entry, roots, manifest_path, install_dir)?;
    crate::hooks::mark_pending_post(&roots.state_dir, &ownership.name)?;
    crate::repo_transition::finish_fresh_recovery(install_dir, ownership, &roots.state_dir)
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

/// Secures clone permissions, skipping the full-tree walk when a previous
/// walk already proved this exact tree.
///
/// The walk stats every entry in the clone (~70k entries across a typical
/// install set) on every no-op update. Fresh updates pull nothing, so a
/// tree whose HEAD revision and root mtime match the last completed walk
/// cannot have gained new files from git; re-walking would only re-prove
/// the same modes. Out-of-band `chmod` inside a managed clone (which
/// updates neither HEAD nor mtime) is still repaired by the next
/// non-fresh update or pull, exactly as before.
///
/// Stamp writes are best-effort: a read-only state dir must not turn a
/// previously successful fresh update into a failure.
#[cfg(unix)]
fn secure_managed_clone_permissions_cached(
    state_dir: &Path,
    name: &str,
    install_dir: &Path,
) -> Result<()> {
    if permwalk_stamp_current(state_dir, name, install_dir) {
        return Ok(());
    }
    secure_managed_clone_permissions(install_dir)?;
    let _ = write_permwalk_stamp(state_dir, name, install_dir);
    Ok(())
}

/// Non-Unix permission walks are already no-ops; keep them stamp-free so
/// no new state files appear on platforms with nothing to skip.
#[cfg(not(unix))]
fn secure_managed_clone_permissions_cached(
    _state_dir: &Path,
    _name: &str,
    install_dir: &Path,
) -> Result<()> {
    secure_managed_clone_permissions(install_dir)
}

#[cfg(unix)]
fn write_permwalk_stamp(state_dir: &Path, name: &str, install_dir: &Path) -> Result<()> {
    let Some(fingerprint) = permwalk_fingerprint(install_dir) else {
        return Ok(());
    };
    state::write_atomic(
        &permwalk_stamp_path(state_dir, name),
        &format!(
            "1\n{}\n{}\n{}\n",
            fingerprint.head, fingerprint.mtime_secs, fingerprint.mtime_nanos
        ),
    )
}

#[cfg(not(unix))]
fn write_permwalk_stamp(_state_dir: &Path, _name: &str, _install_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn permwalk_stamp_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.permwalk.stamp"))
}

#[cfg(unix)]
fn permwalk_stamp_current(state_dir: &Path, name: &str, install_dir: &Path) -> bool {
    let Some(current) = permwalk_fingerprint(install_dir) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(permwalk_stamp_path(state_dir, name)) else {
        return false;
    };
    permwalk_stamp_parse(&text) == Some(current)
}

#[cfg(not(unix))]
fn permwalk_stamp_current(_state_dir: &Path, _name: &str, _install_dir: &Path) -> bool {
    false
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct PermwalkFingerprint {
    head: String,
    mtime_secs: u64,
    mtime_nanos: u32,
}

#[cfg(unix)]
fn permwalk_fingerprint(install_dir: &Path) -> Option<PermwalkFingerprint> {
    let head = git_head_direct(install_dir)?;
    let mtime = fs::metadata(install_dir)
        .and_then(|meta| meta.modified())
        .ok()?;
    let since_epoch = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(PermwalkFingerprint {
        head,
        mtime_secs: since_epoch.as_secs(),
        mtime_nanos: since_epoch.subsec_nanos(),
    })
}

#[cfg(unix)]
fn permwalk_stamp_parse(text: &str) -> Option<PermwalkFingerprint> {
    let mut lines = text.lines();
    if lines.next()? != "1" {
        return None;
    }
    let head = lines.next()?;
    if head.is_empty() {
        return None;
    }
    let mtime_secs = lines.next()?.parse::<u64>().ok()?;
    let mtime_nanos = lines.next()?.parse::<u32>().ok()?;
    if lines.next().is_some() {
        return None;
    }
    Some(PermwalkFingerprint {
        head: head.to_owned(),
        mtime_secs,
        mtime_nanos,
    })
}

/// Resolves HEAD without spawning git, for stamp fingerprints.
///
/// Reads the loose ref first (the common case after clone/pull) and falls
/// back to `packed-refs`. Any failure returns None and the caller walks;
/// correctness never depends on this succeeding.
#[cfg(unix)]
fn git_head_direct(install_dir: &Path) -> Option<String> {
    let git_dir = git_dir_for(install_dir)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(refname) = head.strip_prefix("ref: ") {
        // Absolute targets would escape the git dir via `join` (same-user,
        // equality-only use, but there is no reason to allow them).
        if refname.is_empty() || refname.contains("..") || refname.starts_with('/') {
            return None;
        }
        if let Ok(sha) = fs::read_to_string(git_dir.join(refname)) {
            let sha = sha.trim();
            if !sha.is_empty() {
                return Some(sha.to_owned());
            }
        }
        let packed = fs::read_to_string(git_dir.join("packed-refs")).ok()?;
        for line in packed.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            if let (Some(sha), Some(name)) = (parts.next(), parts.next()) {
                if name == refname && !sha.is_empty() {
                    return Some(sha.to_owned());
                }
            }
        }
        return None;
    }
    if head.is_empty() {
        return None;
    }
    Some(head.to_owned())
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
    record_success_with_roots(entry, context.roots, context.manifest_path, install_dir)
}

fn record_success_with_roots(
    entry: &Entry,
    roots: &crate::runtime::Roots,
    manifest_path: &Path,
    install_dir: &Path,
) -> Result<()> {
    cancellation::check()?;
    // Public links, extras, and the manifest row are one publication unit.
    // Once entered, finish it so the existing recovery metadata remains
    // authoritative rather than interrupting between its durable writes.
    bin_link::from_dir(&roots.state_dir, &roots.bin_dir, &entry.name, install_dir)?;
    extras::link(
        &roots.state_dir,
        &roots.install_dir,
        &entry.name,
        install_dir,
    )?;
    manifest::upsert(
        manifest_path,
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

/// Pulls a development clone; `Err` carries Git's stderr for the warning.
fn development_pull(
    verified: &repo_verify::VerifiedDevelopment,
    runner: &impl Runner,
    dir: &Path,
) -> Result<std::result::Result<(), String>> {
    let output = verified.run_pull(dir, runner)?;
    Ok(if output.success {
        Ok(())
    } else {
        Err(output.stderr)
    })
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

fn local_pull_failure_detail(status: GitStatus, stderr: &str) -> String {
    // A development clone is user-owned and pulled in one bounded command, so
    // only the dirty case is classified; otherwise Git's own first line is
    // the cause rather than a guess.
    let cause = if status.dirty {
        "dirty working tree".to_owned()
    } else {
        stale_remote::first_line(stderr)
    };
    if cause.is_empty() {
        "pull failed (local clone)".to_owned()
    } else {
        format!("pull failed ({cause}; local clone)")
    }
}

fn git_head(runner: &impl Runner, dir: &Path) -> Option<String> {
    git(runner, dir, &["rev-parse", "HEAD"]).map(|output| output.stdout.trim().to_owned())
}

/// Bound on the alternate-transport fetch retry. Generous for a shallow
/// clone's incremental fetch; its real job is keeping the retry off the
/// terminal (see [`fetch_via_alternate_origin`]).
const ALTERNATE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Fetches, then fast-forwards, an existing managed checkout.
///
/// One `pull --ff-only` cannot say which half failed, and its exit status is
/// all the old code kept, so a fetch that never reached the network was
/// reported as "no fast-forward". Split, each half reports its own cause.
fn refresh(
    runner: &impl Runner,
    dir: &Path,
    configured_url: &str,
) -> std::result::Result<(), stale_remote::Failure> {
    fetch(runner, dir, configured_url)?;
    let merge = git_output(
        runner,
        dir,
        &["merge", "--ff-only", "--quiet", "@{upstream}"],
    );
    if merge.as_ref().is_some_and(|output| output.success) {
        return Ok(());
    }
    let stderr = merge.map(|output| output.stderr).unwrap_or_default();
    // Divergence is definitive (no fast-forward exists whatever the tree
    // holds) and needs a new clone, so it wins over a dirty tree, which may
    // only be untracked build output. An unreported status means the cause
    // genuinely cannot be classified.
    let reason = if has_unpublished_commits(runner, dir) {
        stale_remote::Reason::Diverged
    } else {
        let status = git_status(runner, dir);
        if !status.reported {
            stale_remote::Reason::Status
        } else if status.dirty {
            stale_remote::Reason::Dirty
        } else {
            stale_remote::Reason::Merge
        }
    };
    Err(stale_remote::Failure::new(reason, &stderr))
}

/// Whether HEAD has commits its upstream lacks, so no fast-forward exists.
/// Exit codes and localized messages are not portable signals; a commit
/// count is. In a shallow clone a rewritten upstream shares no visible
/// history, which counts the local tip and correctly reads as diverged.
fn has_unpublished_commits(runner: &impl Runner, dir: &Path) -> bool {
    git(runner, dir, &["rev-list", "--count", "@{upstream}..HEAD"])
        .and_then(|output| output.stdout.trim().parse::<u64>().ok())
        .is_some_and(|count| count > 0)
}

/// Fetches the checkout's upstream, retrying once over the other GitHub
/// transport. The reported failure is always the current origin's: that is
/// the remote the checkout keeps using, so it is the one to act on.
fn fetch(
    runner: &impl Runner,
    dir: &Path,
    configured_url: &str,
) -> std::result::Result<(), stale_remote::Failure> {
    let primary = git_output(runner, dir, &["fetch", "--quiet"]);
    if primary.as_ref().is_some_and(|output| output.success) {
        return Ok(());
    }
    if fetch_via_alternate_origin(runner, dir, configured_url) {
        return Ok(());
    }
    let stderr = primary.map(|output| output.stderr).unwrap_or_default();
    let reason = if upstream_branch_gone(runner, dir) {
        stale_remote::Reason::UpstreamGone
    } else {
        stale_remote::Reason::Fetch
    };
    Err(stale_remote::Failure::new(reason, &stderr))
}

/// Bound on the deleted-upstream probe. It lists one ref, so a healthy
/// remote answers in well under a second; the bound keeps an unreachable
/// one from doubling a hung fetch's wait and runs the probe off the
/// terminal so it cannot prompt.
const UPSTREAM_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether the remote answered but no longer has the branch HEAD tracks.
///
/// A managed clone is shallow, so its fetch refspec names that one branch
/// and fails for good once the branch is deleted or the default branch is
/// renamed. Git's message is localized and not a stable signal; an empty
/// `ls-remote` listing from a remote that answered is. Any doubt (detached
/// HEAD, no upstream, unreachable remote) reads as an ordinary fetch
/// failure. Only runs after the fetch failed on every transport.
fn upstream_branch_gone(runner: &impl Runner, dir: &Path) -> bool {
    let config = |key: String| {
        git(runner, dir, &["config", "--get", &key])
            .map(|output| output.stdout.trim().to_owned())
            .filter(|value| !value.is_empty() && !value.starts_with('-'))
    };
    let Some(branch) = git(runner, dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|output| output.stdout.trim().to_owned())
    else {
        return false;
    };
    let (Some(remote), Some(merge)) = (
        config(format!("branch.{branch}.remote")),
        config(format!("branch.{branch}.merge")),
    ) else {
        return false;
    };
    git_output_bounded(
        runner,
        dir,
        &["ls-remote", &remote, &merge],
        Some(UPSTREAM_PROBE_TIMEOUT),
    )
    .is_some_and(|output| output.success && !output.timed_out && output.stdout.trim().is_empty())
}

/// Retries the fetch over the other transport of the configured GitHub URL
/// and keeps that origin only if the fetch succeeded.
///
/// The old fallback switched HTTPS to SSH for good after any failed pull,
/// including a network blip or a diverged tree, and never switched back, so
/// on a host without a GitHub SSH key one blip stranded the checkout on a
/// remote it could never reach. The rule now: origin only moves to a URL
/// that just worked, otherwise it is restored. That still adopts SSH for a
/// repository that turned private, heals a checkout stranded by the old
/// code once HTTPS works, and leaves a non-GitHub or explicit SSH origin
/// alone.
///
/// The retry is bounded, which also runs it detached from the terminal: a
/// fallback the user did not choose must fail fast rather than stop an
/// interactive update at a credential or host-key prompt.
fn fetch_via_alternate_origin(runner: &impl Runner, dir: &Path, configured_url: &str) -> bool {
    // The stored value, not `remote get-url`: an `insteadOf` rewrite would
    // otherwise be written back over the configured URL on restore.
    let Some(origin) = git(runner, dir, &["config", "--get", "remote.origin.url"])
        .map(|output| output.stdout.trim().to_owned())
    else {
        return false;
    };
    let Some(alternate) = alternate_origin(&origin, configured_url) else {
        return false;
    };
    if git(runner, dir, &["remote", "set-url", "origin", &alternate]).is_none() {
        return false;
    }
    let retry = git_output_bounded(
        runner,
        dir,
        &["fetch", "--quiet"],
        Some(ALTERNATE_FETCH_TIMEOUT),
    );
    if retry.is_some_and(|output| output.success) {
        if let Some(ssh) = repo::ssh_fallback(configured_url) {
            set_push_url(runner, dir, &ssh);
        }
        return true;
    }
    // Best effort: if this is interrupted, the next run's failed fetch over
    // the alternate retries the original transport and moves back on success.
    let _ = git(runner, dir, &["remote", "set-url", "origin", &origin]);
    false
}

/// The other transport for `origin`, only within the configured GitHub URL
/// and its SSH form. `None` for anything else: an explicit SSH or
/// non-GitHub override is the user's choice, not a fallback candidate.
fn alternate_origin(origin: &str, configured_url: &str) -> Option<String> {
    let ssh = repo::ssh_fallback(configured_url)?;
    if origin == ssh {
        Some(configured_url.to_owned())
    } else {
        (repo::ssh_fallback(origin).as_deref() == Some(ssh.as_str())).then_some(ssh)
    }
}

fn remote_origin(runner: &impl Runner, dir: &Path) -> Option<String> {
    git(runner, dir, &["remote", "get-url", "origin"]).map(|output| output.stdout.trim().to_owned())
}

fn sync_ssh_push_url(runner: &impl Runner, install_dir: &Path) -> bool {
    // System-level config is assumed at `/etc/gitconfig`: a nonstandard-prefix
    // git (e.g. Homebrew macOS, whose system config lives under the prefix)
    // with a system-level `insteadOf` would be under-collected and could skip
    // a sync git would perform. Vanishingly rare; everything else about
    // collection (missing HOME, `~user/` includes, conditional includes,
    // unparsable lines) fails closed to the slow path instead.
    sync_ssh_push_url_with_configs(
        runner,
        install_dir,
        &user_git_config_paths(),
        Path::new("/etc/gitconfig"),
        std::env::var_os("HOME").map(PathBuf::from),
        &git_config_env_snapshot(),
    )
}

fn sync_ssh_push_url_with_configs(
    runner: &impl Runner,
    install_dir: &Path,
    user_configs: &[PathBuf],
    system_config: &Path,
    home: Option<PathBuf>,
    env: &GitConfigEnv,
) -> bool {
    if !env.hard_override
        && push_url_sync_redundant_with_configs(
            install_dir,
            user_configs,
            system_config,
            home.as_deref(),
            &env.count_rewrites,
        )
    {
        return false;
    }
    let Some(origin) = remote_origin(runner, install_dir) else {
        return false;
    };
    if let Some(fallback) = expected_push_url(&origin) {
        if remote_push_url(runner, install_dir).as_deref() == Some(fallback.as_str()) {
            return false;
        }
        return set_push_url(runner, install_dir, &fallback);
    }
    false
}

/// `GIT_*` environment state relevant to the push-url fast path.
///
/// Path-redirecting variables (`GIT_DIR`, custom global/system config paths)
/// defeat local reads entirely. `GIT_CONFIG_COUNT` pairs act like repeated
/// `-c` flags, so their `insteadOf` entries join the collected rewrite rules
/// (as `(base, prefix)` pairs) and are applied like file-based ones;
/// anything else in those pairs that could affect the origin answer forces
/// the slow path.
#[derive(Debug, Default)]
struct GitConfigEnv {
    hard_override: bool,
    count_rewrites: Vec<(String, String)>,
}

fn git_config_env_snapshot() -> GitConfigEnv {
    let mut env = GitConfigEnv {
        hard_override: false,
        count_rewrites: Vec::new(),
    };
    let vars: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.as_encoded_bytes().to_vec(),
                value.as_encoded_bytes().to_vec(),
            )
        })
        .collect();
    for key in vars.keys() {
        if key == b"GIT_DIR"
            || key == b"GIT_WORK_TREE"
            || key == b"GIT_COMMON_DIR"
            || key == b"GIT_CONFIG_GLOBAL"
            || key == b"GIT_CONFIG_SYSTEM"
            || key == b"GIT_CONFIG_PARAMETERS"
        {
            env.hard_override = true;
        } else if key.starts_with(b"GIT_CONFIG") {
            // Recognized precisely below (`COUNT`/`KEY_*`/`VALUE_*`) or
            // rejected as unknown. `NOSYSTEM` only narrows what git reads,
            // so continuing to scan the system file stays conservative.
            if key == b"GIT_CONFIG_COUNT"
                || key == b"GIT_CONFIG_NOSYSTEM"
                || key.starts_with(b"GIT_CONFIG_KEY_")
                || key.starts_with(b"GIT_CONFIG_VALUE_")
            {
                continue;
            }
            env.hard_override = true;
        }
    }
    if env.hard_override {
        return env;
    }
    let count = match vars.get(b"GIT_CONFIG_COUNT".as_slice()) {
        None => 0,
        Some(raw) => match String::from_utf8_lossy(raw).trim().parse::<usize>() {
            Ok(count) => count,
            Err(_) => {
                env.hard_override = true;
                return env;
            }
        },
    };
    for index in 0..count {
        let key = vars.get(format!("GIT_CONFIG_KEY_{index}").as_bytes());
        let value = vars.get(format!("GIT_CONFIG_VALUE_{index}").as_bytes());
        let (Some(key), Some(value)) = (key, value) else {
            env.hard_override = true;
            return env;
        };
        let key = String::from_utf8_lossy(key);
        let lowered = key.to_ascii_lowercase();
        if lowered.starts_with("url.") && lowered.ends_with(".insteadof") {
            let base = key["url.".len()..key.len() - ".insteadof".len()].to_owned();
            if base.is_empty() {
                env.hard_override = true;
                return env;
            }
            env.count_rewrites
                .push((base, String::from_utf8_lossy(value).into_owned()));
        } else if lowered.starts_with("url.") && lowered.ends_with(".pushinsteadof") {
            // Push-side rewriting never changes the fetch answer the push
            // URL is derived from.
        } else if lowered == "remote.origin.url" || lowered.starts_with("include") {
            env.hard_override = true;
            return env;
        } else if lowered.starts_with("url.") || lowered.starts_with("remote.origin.") {
            // Unrecognized URL-affecting keys fail closed.
            env.hard_override = true;
            return env;
        }
    }
    env
}

/// Computes the push URL `sync_ssh_push_url` converges a clone to.
///
/// Single owner of the fetch→push mapping so the git-based sync and the
/// config-read fast path below cannot drift apart.
fn expected_push_url(origin: &str) -> Option<String> {
    if origin.starts_with("git@github.com:") {
        Some(origin.to_owned())
    } else {
        repo::ssh_fallback(origin)
    }
}

/// Returns whether the push-url sync would be a no-op, without spawning git.
///
/// Every no-op update runs this per repo dependency (up to three git spawns
/// each: `get-url`, a `get-url --push` pre-read, and `set-url --push` when
/// stale). A converged clone already carries the expected push URL in
/// `.git/config`, so read that file directly and skip all three spawns.
/// Fail-closed: any ambiguity returns false and the git-based sync runs
/// unchanged, so the worst case is today's cost, never a wrong skip.
fn push_url_sync_redundant_with_configs(
    install_dir: &Path,
    user_configs: &[PathBuf],
    system_config: &Path,
    home: Option<&Path>,
    count_rewrites: &[(String, String)],
) -> bool {
    let Some(git_dir) = git_dir_for(install_dir) else {
        return false;
    };
    let repo_config = git_dir.join("config");
    let Ok(repo_text) = fs::read_to_string(&repo_config) else {
        return false;
    };
    let Some(state) = parse_origin_push_state(&repo_text) else {
        return false;
    };
    if state.urls.len() != 1 {
        // No repo-level `url` means the fetch URL (if any) comes from a
        // broader-scope file, and several means git's last-wins pick is not
        // worth replicating. Either way the slow path decides.
        return false;
    }
    // `insteadOf` rewriting is the one repo-external state that can change
    // the fetch answer the slow path derives its push URL from. Broader
    // `pushurl` entries cannot: the slow path only ever rewrites the
    // repo-level value, so a repo file that already matches is untouched
    // either way. Rather than bailing on any matching rule, apply git's
    // longest-match rewrite and compare outcomes: a rewrite that leaves the
    // derived push URL unchanged (the common scp↔https round-trip) still
    // permits the skip.
    let mut rewrites = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let mut extra = user_configs.to_vec();
    extra.push(system_config.to_path_buf());
    for path in std::iter::once(&repo_config).chain(extra.iter()) {
        if !collect_rewrites(path, home, &mut rewrites, &mut visited, 0) {
            return false;
        }
    }
    // A linked worktree's shared config can also carry rewrites.
    if let Ok(commondir) = fs::read_to_string(git_dir.join("commondir")) {
        let common = git_dir.join(commondir.trim());
        if !collect_rewrites(&common.join("config"), home, &mut rewrites, &mut visited, 0) {
            return false;
        }
    }
    // Worktree-local overrides are read for linked checkouts; scanning them
    // unconditionally is a conservative superset for ordinary clones.
    if git_dir.join("config.worktree").is_file()
        && !collect_rewrites(
            &git_dir.join("config.worktree"),
            home,
            &mut rewrites,
            &mut visited,
            0,
        )
    {
        return false;
    }
    rewrites.extend(count_rewrites.iter().cloned());
    let origin = &state.urls[0];
    let Some(rewritten) = apply_rewrites(origin, &rewrites) else {
        return false;
    };
    match (expected_push_url(origin), expected_push_url(&rewritten)) {
        // The slow path also leaves the config untouched in this case (it
        // only ever reads the origin), so skipping is end-state identical.
        (None, None) => true,
        (Some(raw), Some(rewritten)) if raw == rewritten => state.push_urls == vec![raw],
        // The rewrite changes the derived push URL (or its application is
        // ambiguous), so only git's own answer can decide.
        _ => false,
    }
}

/// Applies git's longest-match `insteadOf` rewrite to a fetch URL.
///
/// Matching is a case-sensitive prefix match like git's. Returns None when
/// the longest match is not unique or the rewritten result still matches a
/// rule (a possible chained rewrite), in which case the caller takes the
/// git-based slow path instead of guessing git's pick.
fn apply_rewrites(origin: &str, rewrites: &[(String, String)]) -> Option<String> {
    let mut longest: Option<(&str, &str)> = None;
    for (base, prefix) in rewrites {
        if prefix.is_empty() || base.is_empty() {
            // Degenerate rules (match-everything prefix, prefix-stripping
            // base) are not replicated; the slow path decides.
            return None;
        }
        if !origin.starts_with(prefix) {
            continue;
        }
        match longest {
            None => longest = Some((base, prefix)),
            Some((_, current)) if prefix.len() > current.len() => longest = Some((base, prefix)),
            // Equal-length matches are necessarily identical prefixes;
            // competing bases for one prefix leave git's pick unclear.
            Some((current_base, current))
                if prefix.len() == current.len() && base != current_base =>
            {
                return None;
            }
            _ => {}
        }
    }
    let Some((base, prefix)) = longest else {
        return Some(origin.to_owned());
    };
    let rewritten = format!("{base}{}", &origin[prefix.len()..]);
    if rewrites
        .iter()
        .any(|(_, rule)| !rule.is_empty() && rewritten.starts_with(rule))
    {
        return None;
    }
    Some(rewritten)
}

/// Collects `insteadOf` URL-rewrite rules from a config file and its
/// non-conditional includes, as `(base, prefix)` pairs. Returns false on
/// anything unclassifiable (conditional includes, unresolvable paths,
/// syntax errors), in which case the caller takes the git-based slow path.
fn collect_rewrites(
    path: &Path,
    home: Option<&Path>,
    rewrites: &mut Vec<(String, String)>,
    visited: &mut std::collections::BTreeSet<PathBuf>,
    depth: usize,
) -> bool {
    if depth > 8 {
        return false;
    }
    if !visited.insert(path.to_path_buf()) {
        // Already collected (diamond or cyclic includes): the rules are in
        // the accumulator, so re-entering would only loop.
        return true;
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        // Missing files are tolerated like git tolerates a dangling
        // include: there is simply no rewrite to collect here. Any other
        // read failure (permissions, non-UTF-8) could hide a rule, so it
        // fails closed instead.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    if text.lines().any(|line| line.trim_end().ends_with('\\')) {
        return false;
    }
    let mut section = String::new();
    let mut url_base: Option<String> = None;
    let mut includes = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let (name, subsection) = match parse_section_header(line) {
                Some(header) => header,
                None => return false,
            };
            // Conditional includes cannot be evaluated without the full
            // gitdir/branch context; fail closed instead of guessing.
            if name.eq_ignore_ascii_case("includeif") {
                return false;
            }
            if name.eq_ignore_ascii_case("url") {
                url_base = subsection;
            } else {
                url_base = None;
            }
            section = name;
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => return false,
        };
        let key = key.trim();
        let Some(value) = parse_config_value(value.trim()) else {
            return false;
        };
        if section.eq_ignore_ascii_case("url") && key.eq_ignore_ascii_case("insteadof") {
            let Some(base) = url_base.clone() else {
                // A baseless `[url]` rule has no well-defined
                // substitution; the slow path decides.
                return false;
            };
            rewrites.push((base, value));
        } else if section.eq_ignore_ascii_case("include") && key.eq_ignore_ascii_case("path") {
            includes.push(value);
        }
    }
    for include in includes {
        let Some(resolved) = resolve_include_path(path, &include, home) else {
            return false;
        };
        if !collect_rewrites(&resolved, home, rewrites, visited, depth + 1) {
            return false;
        }
    }
    true
}

fn resolve_include_path(including: &Path, raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = home?;
        return Some(home.join(rest));
    }
    if raw == "~" {
        return home.map(Path::to_path_buf);
    }
    if raw.starts_with('~') {
        // `~user/` expansion needs passwd lookup; fail closed.
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Some(path);
    }
    including.parent().map(|dir| dir.join(path))
}

/// Returns the user-level gitconfig paths git consults for URL state.
fn user_git_config_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let xdg = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("git/config"),
        _ => home.join(".config/git/config"),
    };
    vec![home.join(".gitconfig"), xdg]
}

/// Resolves the git dir for a checkout, following `gitdir:` pointers.
fn git_dir_for(install_dir: &Path) -> Option<PathBuf> {
    let dotgit = install_dir.join(".git");
    let metadata = fs::symlink_metadata(&dotgit).ok()?;
    if metadata.is_dir() {
        return Some(dotgit);
    }
    if metadata.is_file() {
        let text = fs::read_to_string(&dotgit).ok()?;
        let target = text.strip_prefix("gitdir:")?.trim();
        if target.is_empty() {
            return None;
        }
        let path = PathBuf::from(target);
        let resolved = if path.is_absolute() {
            path
        } else {
            install_dir.join(path)
        };
        if resolved.is_dir() {
            return Some(resolved);
        }
    }
    None
}

struct OriginPushState {
    urls: Vec<String>,
    push_urls: Vec<String>,
}

/// Parses `[remote "origin"]` url/pushurl entries from a repo gitconfig.
///
/// Returns None when the text cannot be classified exactly; the caller then
/// falls back to the git-based sync. Only the quoted `[remote "origin"]`
/// form is recognized: anything else that git might treat as the origin
/// section (unrecognized syntax, dot-form subsections) simply yields no
/// `url`, which the caller also treats as "cannot prove redundant".
fn parse_origin_push_state(text: &str) -> Option<OriginPushState> {
    // A trailing backslash continues the logical line, so a following
    // `[section]` line may be a value continuation rather than a header.
    // Continuations are rare in these files; bail instead of replicating
    // git's line-joining rules.
    if text.lines().any(|line| line.trim_end().ends_with('\\')) {
        return None;
    }
    let mut state = OriginPushState {
        urls: Vec::new(),
        push_urls: Vec::new(),
    };
    let mut in_origin = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let (section, subsection) = parse_section_header(line)?;
            in_origin =
                section.eq_ignore_ascii_case("remote") && subsection.as_deref() == Some("origin");
            continue;
        }
        if !in_origin {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let value = parse_config_value(value.trim())?;
        if key.trim().eq_ignore_ascii_case("url") {
            state.urls.push(value);
        } else if key.trim().eq_ignore_ascii_case("pushurl") {
            state.push_urls.push(value);
        }
    }
    Some(state)
}

fn parse_section_header(line: &str) -> Option<(String, Option<String>)> {
    let inner = line.strip_prefix('[')?;
    let mut in_quotes = false;
    let mut escaped = false;
    let mut end = None;
    for (index, byte) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ']' if !in_quotes => {
                end = Some(index);
                break;
            }
            _ => {}
        }
    }
    let end = end?;
    if in_quotes {
        return None;
    }
    let rest = inner[end + 1..].trim_start();
    if !(rest.is_empty() || rest.starts_with('#') || rest.starts_with(';')) {
        return None;
    }
    let header = inner[..end].trim();
    let Some(split) = header.find(char::is_whitespace) else {
        return Some((header.to_owned(), None));
    };
    let (section, subsection) = header.split_at(split);
    let subsection = subsection.trim();
    if let Some(quoted) = subsection
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return Some((section.to_owned(), Some(decode_config_quoted(quoted)?)));
    }
    Some((section.to_owned(), Some(subsection.to_owned())))
}

fn parse_config_value(text: &str) -> Option<String> {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut decoded = String::new();
        let mut chars = quoted.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => decoded.push('\n'),
                    't' => decoded.push('\t'),
                    '"' => decoded.push('"'),
                    '\\' => decoded.push('\\'),
                    _ => return None,
                },
                char => decoded.push(char),
            }
        }
        let rest: String = chars.collect();
        let rest = rest.trim_start();
        if !(rest.is_empty() || rest.starts_with('#') || rest.starts_with(';')) {
            return None;
        }
        return Some(decoded);
    }
    // Unquoted values end at a comment marker, but `#`/`;` handling mid-word
    // differs subtly across git versions, so bail rather than guess.
    if text.contains(['#', ';', '"', '\\']) {
        return None;
    }
    Some(text.trim_end().to_owned())
}

fn decode_config_quoted(text: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            decoded.push(char);
            continue;
        }
        match chars.next()? {
            '"' => decoded.push('"'),
            '\\' => decoded.push('\\'),
            _ => return None,
        }
    }
    Some(decoded)
}

fn set_ssh_push_url(runner: &impl Runner, install_dir: &Path, url: &str) {
    if let Some(fallback) = repo::ssh_fallback(url) {
        let _ = set_push_url(runner, install_dir, &fallback);
    }
}

fn remote_push_url(runner: &impl Runner, install_dir: &Path) -> Option<String> {
    git(
        runner,
        install_dir,
        &["remote", "get-url", "--push", "origin"],
    )
    .filter(|output| output.success)
    .map(|output| output.stdout.trim().to_owned())
}

fn set_push_url(runner: &impl Runner, install_dir: &Path, url: &str) -> bool {
    git(
        runner,
        install_dir,
        &["remote", "set-url", "--push", "origin", url],
    )
    .is_some_and(|output| output.success)
}

fn clone_repo(runner: &impl Runner, url: &str, target: &Path) -> bool {
    let target = target.display().to_string();
    runner
        .run("git", &["clone", "--depth", "1", url, &target], None)
        .ok()
        .is_some_and(|output| output.success)
}

fn git(runner: &impl Runner, dir: &Path, args: &[&str]) -> Option<crate::process::Output> {
    git_output(runner, dir, args).filter(|output| output.success)
}

/// Runs `git -C dir args` and keeps the output even when Git fails, so
/// callers that report a failure can quote Git's stderr.
fn git_output(runner: &impl Runner, dir: &Path, args: &[&str]) -> Option<crate::process::Output> {
    git_output_bounded(runner, dir, args, None)
}

/// [`git_output`] with an optional bound. A bounded command runs in its own
/// session without the controlling terminal, so it cannot prompt.
fn git_output_bounded(
    runner: &impl Runner,
    dir: &Path,
    args: &[&str],
    timeout: Option<std::time::Duration>,
) -> Option<crate::process::Output> {
    let dir = dir.display().to_string();
    let mut full = Vec::with_capacity(args.len() + 2);
    full.push("-C");
    full.push(dir.as_str());
    full.extend_from_slice(args);
    runner.run("git", &full, timeout).ok()
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{
        GitConfigEnv, install_existing, push_url_sync_redundant_with_configs,
        secure_managed_clone_permissions, sync_ssh_push_url, sync_ssh_push_url_with_configs,
        upstream_branch_gone,
    };
    use crate::config::Entry;
    use crate::hooks::BashCustomProbe;
    use crate::http::Client;
    use crate::manifest;
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;
    use crate::stamp;
    use crate::update::{Context, ItemReason, ItemStatus, Options};

    const NOW: u64 = 1_700_000_000;

    #[derive(Debug, Default)]
    struct FakeRunner {
        outputs: BTreeMap<(String, Vec<String>), Output>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn with_output<const N: usize>(
            mut self,
            program: &str,
            args: [&str; N],
            success: bool,
            stdout: &str,
        ) -> Self {
            self.outputs.insert(
                (
                    program.to_owned(),
                    args.into_iter().map(str::to_owned).collect(),
                ),
                Output {
                    success,
                    timed_out: false,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                },
            );
            self
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }

        fn git_calls(&self) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .filter(|(program, _)| program == "git")
                .map(|(_, args)| args)
                .collect()
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, _command: &str) -> bool {
            false
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            let owned: Vec<String> = args.iter().copied().map(str::to_owned).collect();
            self.calls
                .lock()
                .unwrap()
                .push((program.to_owned(), owned.clone()));
            self.outputs
                .get(&(program.to_owned(), owned))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
        }
    }

    #[derive(Debug, Default)]
    struct NoClient;

    impl Client for NoClient {
        fn get(&self, _url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no network in tests",
            ))
        }
    }

    struct Fixture {
        roots: Roots,
        hooks: BashCustomProbe,
        env: RuntimeEnv,
        env_vars: BTreeMap<String, String>,
        client: NoClient,
        manifest_path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let home = crate::test_support::temp_dir(&format!("shdeps-update-repo-{name}"));
            let roots = Roots {
                conf_dir: home.join("conf"),
                hooks_dir: home.join("conf/hooks.d"),
                state_dir: home.join("state"),
                git_dev_dir: home.join("git"),
                install_dir: home.join("share"),
                bin_dir: home.join("bin"),
                home: home.clone(),
            };
            fs::create_dir_all(&roots.state_dir).unwrap();
            fs::create_dir_all(&roots.install_dir).unwrap();
            let manifest_path = manifest::path(&roots.state_dir);
            Self {
                roots,
                hooks: BashCustomProbe::new(home.join("shdeps.sh")),
                env: RuntimeEnv::new("linux", "host"),
                env_vars: BTreeMap::new(),
                client: NoClient,
                manifest_path,
            }
        }

        fn context<'a, R: Runner>(&'a self, runner: &'a R, pkg_mgr: &'a str) -> Context<'a, R> {
            Context {
                manifest_path: &self.manifest_path,
                roots: &self.roots,
                env: &self.env,
                hooks: &self.hooks,
                runner,
                pkg_mgr,
                env_vars: &self.env_vars,
                client: &self.client,
            }
        }

        fn entry(&self, name: &str) -> Entry {
            Entry {
                name: name.to_owned(),
                method: crate::method::GITHUB_REPO.to_owned(),
                cmd: name.to_owned(),
                cmd_explicit: false,
                aliases: String::new(),
                filter: String::new(),
            }
        }

        fn options() -> Options {
            Options {
                now: NOW,
                remote_ttl: 3600,
                ..Options::default()
            }
        }

        fn install_dir(&self, name: &str) -> PathBuf {
            self.roots.install_dir.join(name)
        }

        fn write_clone(&self, name: &str) -> PathBuf {
            let install_dir = self.install_dir(name);
            let bin = install_dir.join("bin").join(name);
            fs::create_dir_all(install_dir.join(".git")).unwrap();
            fs::create_dir_all(bin.parent().unwrap()).unwrap();
            fs::write(&bin, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            }
            install_dir
        }

        fn write_fresh_stamp(&self, name: &str) {
            let stamp_path = stamp::remote_path(&self.roots.state_dir, name, "repo");
            stamp::remote_touch(&stamp_path, NOW).unwrap();
        }

        fn write_git_config(&self, install_dir: &Path, content: &str) {
            fs::write(install_dir.join(".git").join("config"), content).unwrap();
        }

        fn write_git_head(&self, install_dir: &Path, sha: &str) {
            let git_dir = install_dir.join(".git");
            fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
            fs::write(git_dir.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        }
    }

    fn git_args(install_dir: &Path, args: &[&str]) -> Vec<String> {
        let dir = install_dir.display().to_string();
        let mut full = vec!["-C".to_owned(), dir];
        full.extend(args.iter().map(|arg| (*arg).to_owned()));
        full
    }

    #[test]
    fn upstream_branch_gone_needs_an_answering_remote_without_the_branch() {
        // Only an empty listing from a remote that answered proves the
        // branch is gone; a listed branch, an unreachable remote, a detached
        // HEAD or a missing upstream all stay ordinary fetch failures.
        let dir = Path::new("/share/tool");
        let tracked = || {
            FakeRunner::default()
                .with_output(
                    "git",
                    [
                        "-C",
                        "/share/tool",
                        "symbolic-ref",
                        "--quiet",
                        "--short",
                        "HEAD",
                    ],
                    true,
                    "main\n",
                )
                .with_output(
                    "git",
                    ["-C", "/share/tool", "config", "--get", "branch.main.remote"],
                    true,
                    "origin\n",
                )
                .with_output(
                    "git",
                    ["-C", "/share/tool", "config", "--get", "branch.main.merge"],
                    true,
                    "refs/heads/main\n",
                )
        };
        let listing = [
            "-C",
            "/share/tool",
            "ls-remote",
            "origin",
            "refs/heads/main",
        ];

        assert!(upstream_branch_gone(
            &tracked().with_output("git", listing, true, ""),
            dir
        ));
        assert!(!upstream_branch_gone(
            &tracked().with_output("git", listing, true, "abc123\trefs/heads/main\n"),
            dir
        ));
        assert!(!upstream_branch_gone(
            &tracked().with_output("git", listing, false, ""),
            dir
        ));

        let detached = FakeRunner::default();
        assert!(!upstream_branch_gone(&detached, dir));
        assert_eq!(
            detached.git_calls().len(),
            1,
            "no lookup past a detached HEAD"
        );

        let untracked = FakeRunner::default().with_output(
            "git",
            [
                "-C",
                "/share/tool",
                "symbolic-ref",
                "--quiet",
                "--short",
                "HEAD",
            ],
            true,
            "main\n",
        );
        assert!(!upstream_branch_gone(&untracked, dir));
        assert!(
            !untracked
                .git_calls()
                .iter()
                .any(|args| args.iter().any(|arg| arg == "ls-remote")),
            "no remote probe without an upstream"
        );
    }

    #[test]
    fn sync_push_url_sets_ssh_push_for_https_origin() {
        let fixture = Fixture::new("push-https");
        let install_dir = fixture.write_clone("tool");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                // No pushurl configured: real git prints the fetch URL here.
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        // Stale pre-read: the set runs and reports a mutation.
        assert!(sync_ssh_push_url(&runner, &install_dir));

        assert_eq!(
            runner.git_calls(),
            vec![
                git_args(&install_dir, &["remote", "get-url", "origin"]),
                git_args(&install_dir, &["remote", "get-url", "--push", "origin"]),
                git_args(
                    &install_dir,
                    &[
                        "remote",
                        "set-url",
                        "--push",
                        "origin",
                        "git@github.com:owner/tool.git",
                    ],
                ),
            ]
        );
    }

    #[test]
    fn sync_push_url_reuses_ssh_origin_as_push() {
        let fixture = Fixture::new("push-ssh");
        let install_dir = fixture.write_clone("tool");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "git@github.com:owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                // Stale https pushurl left over from before the origin
                // moved to ssh.
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        assert!(sync_ssh_push_url(&runner, &install_dir));

        assert_eq!(runner.git_calls().len(), 3);
        assert_eq!(
            runner.git_calls()[2],
            git_args(
                &install_dir,
                &[
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
            )
        );
    }

    #[test]
    fn sync_push_url_skips_set_url_for_non_github_origin() {
        let fixture = Fixture::new("push-other");
        let install_dir = fixture.write_clone("tool");
        let runner = FakeRunner::default().with_output(
            "git",
            [
                "-C",
                &install_dir.display().to_string(),
                "remote",
                "get-url",
                "origin",
            ],
            true,
            "https://example.com/owner/tool.git\n",
        );

        sync_ssh_push_url(&runner, &install_dir);

        assert_eq!(
            runner.git_calls(),
            vec![git_args(&install_dir, &["remote", "get-url", "origin"])]
        );
    }

    #[test]
    fn sync_push_url_does_nothing_when_origin_lookup_fails() {
        let fixture = Fixture::new("push-fail");
        let install_dir = fixture.write_clone("tool");
        let runner = FakeRunner::default();

        sync_ssh_push_url(&runner, &install_dir);

        assert_eq!(
            runner.git_calls(),
            vec![git_args(&install_dir, &["remote", "get-url", "origin"])]
        );
    }

    #[cfg(unix)]
    #[test]
    fn secure_clone_permissions_strips_write_bits_and_keeps_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("perms");
        let install_dir = fixture.install_dir("tool");
        let nested = install_dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let loose_file = nested.join("data.txt");
        fs::write(&loose_file, "data").unwrap();
        fs::set_permissions(&loose_file, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o775)).unwrap();
        std::os::unix::fs::symlink("data.txt", nested.join("link")).unwrap();

        secure_managed_clone_permissions(&install_dir).unwrap();

        assert_eq!(
            fs::metadata(&loose_file).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&install_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(nested.join("link")).unwrap(),
            PathBuf::from("data.txt")
        );
    }

    #[test]
    fn fresh_install_existing_reports_current_and_records_state() {
        let fixture = Fixture::new("fresh-current");
        let install_dir = fixture.write_clone("tool");
        fixture.write_fresh_stamp("tool");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );
        let context = fixture.context(&runner, "apt");

        let item = install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &install_dir,
            "https://github.com/owner/tool",
            &mut crate::hooks::MutationIntent::new(&fixture.roots.state_dir, "tool"),
        )
        .unwrap();

        assert_eq!(item.name, "tool");
        assert!(!item.changed);
        assert!(!item.failed);
        assert_eq!(item.status, ItemStatus::Current);
        assert_eq!(item.reason, ItemReason::Fresh);
        assert_eq!(item.detail, "fresh");
        let recorded = manifest::read(&fixture.manifest_path).unwrap();
        let row = recorded
            .get("tool")
            .expect("fresh update must record the manifest row");
        assert_eq!(row.method, crate::method::GITHUB_REPO);
        assert_eq!(row.cmd, "tool");
        assert_eq!(row.install_path, install_dir.display().to_string());
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_dir.join("bin").join("tool")
        );
    }

    #[test]
    fn sync_push_url_with_configs_skips_spawns_when_config_synced() {
        // Perf: a converged clone must cost zero git spawns. The fake runner
        // has no outputs, so any attempted spawn would also fail loudly; the
        // empty call log proves the fast path engaged.
        let fixture = Fixture::new("push-skip");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\
             \turl = https://github.com/owner/tool.git\n\
             \tpushurl = git@github.com:owner/tool.git\n\
             \tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        );
        let runner = FakeRunner::default();

        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        );

        assert!(
            runner.calls().is_empty(),
            "synced clone must not spawn git: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn sync_push_url_with_configs_skips_spawns_for_synced_ssh_origin() {
        let fixture = Fixture::new("push-skip-ssh");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = git@github.com:owner/tool.git\n\
             \tpushurl = git@github.com:owner/tool.git\n",
        );
        let runner = FakeRunner::default();

        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        );

        assert!(runner.calls().is_empty());
    }

    #[test]
    fn sync_push_url_with_configs_skips_spawns_for_non_github_origin() {
        // The slow path only reads such an origin and writes nothing, so the
        // skip is end-state identical while saving the `get-url` spawn.
        let fixture = Fixture::new("push-skip-other");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://example.com/owner/tool.git\n",
        );
        let runner = FakeRunner::default();

        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        );

        assert!(runner.calls().is_empty());
    }

    #[test]
    fn sync_push_url_with_configs_falls_back_when_pushurl_missing() {
        let fixture = Fixture::new("push-unset");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://github.com/owner/tool.git\n",
        );
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                // No pushurl configured: real git prints the fetch URL here.
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        assert!(sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        ));

        assert_eq!(runner.git_calls().len(), 3);
    }

    #[test]
    fn sync_push_url_with_configs_skips_set_when_preread_converged() {
        // Slow-path honesty: the config file is absent so the parse fast
        // path fails closed, but the live push URL already matches. The
        // pre-read must skip the redundant `set-url` AND report no
        // mutation — callers feed the return into `mutation.resolve`, so
        // dropping the pre-read would owe post hooks on every no-op
        // update whose config is not parse-provable.
        let fixture = Fixture::new("push-preread-skip");
        let install_dir = fixture.write_clone("tool");
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                true,
                "git@github.com:owner/tool.git\n",
            );

        assert!(!sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        ));

        assert_eq!(
            runner.git_calls(),
            vec![
                git_args(&install_dir, &["remote", "get-url", "origin"]),
                git_args(&install_dir, &["remote", "get-url", "--push", "origin"]),
            ]
        );
    }

    #[test]
    fn sync_push_url_with_configs_honors_env_rewrite_rules() {
        // `GIT_CONFIG_COUNT` pairs act like `-c` flags. Irrelevant injected
        // rules keep the fast path, as do rules whose rewrite leaves the
        // derived push URL unchanged; an outcome-changing rule forces the
        // slow path, as does a hard override (redirected git dir/config).
        let fixture = Fixture::new("push-env");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
             \tpushurl = git@github.com:o/t.git\n",
        );

        let runner = FakeRunner::default();
        let irrelevant = GitConfigEnv {
            hard_override: false,
            count_rewrites: vec![(
                "https://github.com/".to_owned(),
                "ssh://internal.example/".to_owned(),
            )],
        };
        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &irrelevant,
        );
        assert!(runner.calls().is_empty());

        // scp↔https round-trip: the rewrite fires but the derived push URL
        // is identical, so the skip stays sound.
        let runner = FakeRunner::default();
        let round_trip = GitConfigEnv {
            hard_override: false,
            count_rewrites: vec![(
                "git@github.com:".to_owned(),
                "https://github.com/".to_owned(),
            )],
        };
        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &round_trip,
        );
        assert!(
            runner.calls().is_empty(),
            "an outcome-preserving rewrite must keep the fast path"
        );

        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/o/t.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                // The rule voids the file parse; the live answer is stale.
                true,
                "https://github.com/o/t.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:o/t.git",
                ],
                true,
                "",
            );
        let outcome_changing = GitConfigEnv {
            hard_override: false,
            count_rewrites: vec![(
                "https://example.com/".to_owned(),
                "https://github.com/".to_owned(),
            )],
        };
        assert!(sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &outcome_changing,
        ));
        assert_eq!(
            runner.git_calls().len(),
            3,
            "an outcome-changing env-injected rule must force the slow path"
        );

        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/o/t.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "--push",
                    "origin",
                ],
                // The override voids the file parse; live is stale.
                true,
                "https://github.com/o/t.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:o/t.git",
                ],
                true,
                "",
            );
        let overridden = GitConfigEnv {
            hard_override: true,
            count_rewrites: Vec::new(),
        };
        assert!(sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &overridden,
        ));
        assert_eq!(
            runner.git_calls().len(),
            3,
            "a hard env override must force the slow path"
        );
    }

    #[test]
    fn push_url_redundancy_handles_config_variants() {
        // (config text, redundant?) — every `false` below must take the slow
        // path; every `true` must skip both spawns with identical end state.
        let converged = "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
             \tpushurl = git@github.com:o/t.git\n";
        let cases = [
            (converged.to_owned(), true),
            // Case-insensitive section and keys, exact origin name.
            (
                "[REMOTE \"origin\"]\n\tURL = https://github.com/o/t.git\n\
              \tPushURL = git@github.com:o/t.git\n"
                    .to_owned(),
                true,
            ),
            // Quoted URL value with trailing comment.
            (
                "[remote \"origin\"]\n\turl = \"https://github.com/o/t.git\" # c\n\
              \tpushurl = git@github.com:o/t.git\n"
                    .to_owned(),
                true,
            ),
            // Wrong push URL: the slow path must repair it.
            (
                "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
              \tpushurl = https://github.com/o/t.git\n"
                    .to_owned(),
                false,
            ),
            // Duplicate push URLs: `set-url --push` would collapse them.
            (
                "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
              \tpushurl = git@github.com:o/t.git\n\
              \tpushurl = git@github.com:o/t.git\n"
                    .to_owned(),
                false,
            ),
            // Several fetch URLs: git's last-wins pick is not replicated.
            (
                "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
              \turl = https://github.com/o/t.git\n"
                    .to_owned(),
                false,
            ),
            // No origin section: the fetch URL may live in a broader file.
            ("[core]\n\trepositoryformatversion = 0\n".to_owned(), false),
            // A dangling include carries no rules, like git tolerates it.
            ("[include]\n\tpath = other\n".to_owned() + converged, true),
            // A rewrite that changes the derived push URL defeats the read.
            (
                "[url \"https://example.com/\"]\n\tinsteadOf = https://github.com/\n".to_owned()
                    + converged,
                false,
            ),
            // A rewrite that round-trips to the same push URL still skips.
            (
                "[url \"git@github.com:\"]\n\tinsteadOf = https://github.com/\n".to_owned()
                    + converged,
                true,
            ),
            // A non-matching rewrite is irrelevant.
            (
                "[url \"x\"]\n\tinsteadOf = y\n".to_owned() + converged,
                true,
            ),
            // Unparsable syntax fails closed.
            (
                "[remote \"origin\"\n\turl = https://github.com/o/t.git\n".to_owned(),
                false,
            ),
            (
                "[remote \"origin\"]\n\turl = https://github.com/o/t.git # c\n".to_owned(),
                false,
            ),
            (
                "[remote \"origin\"]\n\turl = https://github.com/o/t.git \\\n\
              \tpushurl = git@github.com:o/t.git\n"
                    .to_owned(),
                false,
            ),
            (
                "[remote \"origin\"]\n\tthis line has no equals\n".to_owned(),
                false,
            ),
        ];
        for (index, (config, expected)) in cases.iter().enumerate() {
            let fixture = Fixture::new(&format!("push-variant-{index}"));
            let install_dir = fixture.write_clone("tool");
            fixture.write_git_config(&install_dir, config);

            assert_eq!(
                push_url_sync_redundant_with_configs(&install_dir, &[], &missing_path(), None, &[]),
                *expected,
                "case {index}:\n{config}"
            );
        }
    }

    #[test]
    fn apply_rewrites_uses_longest_match_and_rejects_ambiguity() {
        // No rule matches: identity.
        assert_eq!(
            super::apply_rewrites("https://github.com/o/t.git", &[]).as_deref(),
            Some("https://github.com/o/t.git")
        );
        // Longest prefix wins.
        let rewrites = vec![
            ("https://x/".to_owned(), "https://".to_owned()),
            (
                "git@github.com:".to_owned(),
                "https://github.com/".to_owned(),
            ),
        ];
        assert_eq!(
            super::apply_rewrites("https://github.com/o/t.git", &rewrites).as_deref(),
            Some("git@github.com:o/t.git")
        );
        // Equal-length competing matches are ambiguous.
        let tie = vec![
            ("a:".to_owned(), "https://".to_owned()),
            ("b:".to_owned(), "https://".to_owned()),
        ];
        assert_eq!(
            super::apply_rewrites("https://github.com/o/t.git", &tie),
            None
        );
        // A result that still matches a rule may chain; decline to guess.
        let chain = vec![
            ("b:".to_owned(), "a:".to_owned()),
            ("c:".to_owned(), "b:".to_owned()),
        ];
        assert_eq!(super::apply_rewrites("a:x", &chain), None);
        // Degenerate rules are not replicated.
        assert_eq!(
            super::apply_rewrites("a:x", &[("b:".to_owned(), String::new())]),
            None
        );
        assert_eq!(
            super::apply_rewrites("a:x", &[(String::new(), "a:".to_owned())]),
            None
        );
        // Matching is case-sensitive like git's: a differently-cased rule
        // does not fire.
        let cased = vec![("X".to_owned(), "HTTPS://".to_owned())];
        assert_eq!(
            super::apply_rewrites("https://github.com/o/t.git", &cased).as_deref(),
            Some("https://github.com/o/t.git")
        );
    }

    #[test]
    fn push_url_redundancy_ignores_irrelevant_external_state() {
        // Broader-scope `pushurl` entries cannot change what the slow path
        // writes (it only rewrites the repo-level value), and `insteadOf`
        // rules that match no prefix of the origin cannot rewrite its
        // answer, so neither defeats the fast path.
        let fixture = Fixture::new("push-external");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
             \tpushurl = git@github.com:o/t.git\n",
        );
        let user_config = fixture.roots.state_dir.join("user.gitconfig");
        fs::write(
            &user_config,
            "[remote \"origin\"]\n\tpushurl = git@github.com:o/t.git\n\
             [url \"x\"]\n\tinsteadOf = y\n",
        )
        .unwrap();
        let system_config = fixture.roots.state_dir.join("system.gitconfig");
        fs::write(&system_config, "[url \"x\"]\n\tinsteadOf = y\n").unwrap();

        assert!(push_url_sync_redundant_with_configs(
            &install_dir,
            &[user_config],
            &system_config,
            None,
            &[],
        ));
    }

    #[test]
    fn push_url_redundancy_bails_on_relevant_rewrite_rules() {
        let fixture = Fixture::new("push-rewrite");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
             \tpushurl = git@github.com:o/t.git\n",
        );

        // A rule whose rewrite changes the derived push URL forces the slow
        // path; only git's own answer can decide then.
        let user_config = fixture.roots.state_dir.join("user.gitconfig");
        fs::write(
            &user_config,
            "[url \"https://example.com/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        assert!(
            !push_url_sync_redundant_with_configs(
                &install_dir,
                std::slice::from_ref(&user_config),
                &missing_path(),
                None,
                &[],
            ),
            "an outcome-changing insteadOf must force the slow path"
        );

        // Rules hiding behind a followed include count too.
        let nested = fixture.roots.state_dir.join("nested.gitconfig");
        fs::write(
            &nested,
            "[url \"https://example.com/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        fs::write(
            &user_config,
            format!("[include]\n\tpath = {}\n", nested.display()),
        )
        .unwrap();
        assert!(
            !push_url_sync_redundant_with_configs(
                &install_dir,
                std::slice::from_ref(&user_config),
                &missing_path(),
                None,
                &[],
            ),
            "an outcome-changing insteadOf behind an include must force the slow path"
        );

        // Conditional includes cannot be evaluated cheaply.
        fs::write(
            &user_config,
            "[includeIf \"gitdir:~/work/\"]\n\tpath = /nonexistent\n",
        )
        .unwrap();
        assert!(
            !push_url_sync_redundant_with_configs(
                &install_dir,
                std::slice::from_ref(&user_config),
                &missing_path(),
                None,
                &[],
            ),
            "includeIf must force the slow path"
        );

        // `~/` includes need a home to resolve against.
        fs::write(&user_config, "[include]\n\tpath = ~/more.gitconfig\n").unwrap();
        assert!(
            !push_url_sync_redundant_with_configs(
                &install_dir,
                &[user_config],
                &missing_path(),
                None,
                &[],
            ),
            "an unresolvable include must force the slow path"
        );
    }

    #[test]
    fn push_url_redundancy_follows_gitdir_pointers() {
        let fixture = Fixture::new("push-gitdir");
        let install_dir = fixture.write_clone("tool");
        let real_git = fixture.roots.state_dir.join("real-git");
        fs::create_dir_all(&real_git).unwrap();
        fs::write(
            real_git.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/o/t.git\n\
             \tpushurl = git@github.com:o/t.git\n",
        )
        .unwrap();
        fs::remove_dir_all(install_dir.join(".git")).unwrap();
        fs::write(
            install_dir.join(".git"),
            format!("gitdir: {}\n", real_git.display()),
        )
        .unwrap();

        assert!(push_url_sync_redundant_with_configs(
            &install_dir,
            &[],
            &missing_path(),
            None,
            &[],
        ));
    }

    #[test]
    fn push_url_redundancy_rejects_missing_git_dir() {
        let fixture = Fixture::new("push-nogit");
        let install_dir = fixture.install_dir("tool");
        fs::create_dir_all(&install_dir).unwrap();

        assert!(!push_url_sync_redundant_with_configs(
            &install_dir,
            &[],
            &missing_path(),
            None,
            &[],
        ));
    }

    #[test]
    fn fresh_install_existing_end_state_matches_with_fast_paths() {
        // Transparency probe through the production entry point: a converged
        // `.git/config` takes the zero-spawn path when the host git
        // environment allows it, and the slow path otherwise, but the
        // recorded end state (item, manifest row, links) is identical either
        // way. No fake git outputs are installed, so a slow-path run simply
        // observes a failed origin lookup and records the same state.
        let fixture = Fixture::new("fresh-fastpath");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_config(
            &install_dir,
            "[remote \"origin\"]\n\turl = https://github.com/owner/tool.git\n\
             \tpushurl = git@github.com:owner/tool.git\n",
        );
        fixture.write_fresh_stamp("tool");
        let runner = FakeRunner::default();
        let context = fixture.context(&runner, "apt");

        let item = install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &install_dir,
            "https://github.com/owner/tool",
            &mut crate::hooks::MutationIntent::new(&fixture.roots.state_dir, "tool"),
        )
        .unwrap();

        assert_eq!(item.reason, ItemReason::Fresh);
        assert_eq!(item.status, ItemStatus::Current);
        let recorded = manifest::read(&fixture.manifest_path).unwrap();
        assert!(recorded.get("tool").is_some());
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_dir.join("bin").join("tool")
        );
    }

    #[cfg(unix)]
    #[test]
    fn permwalk_skips_walk_when_stamp_current() {
        use std::os::unix::fs::PermissionsExt;

        // Perf + transparency: with a valid stamp the tree is not walked, so
        // an out-of-band chmod (which changes neither HEAD nor mtime) is
        // deliberately NOT repaired here. Repair still happens on the next
        // non-fresh update; this test pins the skip so the deferral cannot
        // silently turn into an unconditional walk (or vice versa).
        let fixture = Fixture::new("permwalk-skip");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_head(&install_dir, "abc123");
        let probe = install_dir.join("nested").join("data.txt");
        fs::create_dir_all(probe.parent().unwrap()).unwrap();
        fs::write(&probe, "data").unwrap();
        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o666)).unwrap();

        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();

        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o666,
            "a current stamp must skip the walk (chmod repair is deferred)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn permwalk_walks_and_stamps_when_stamp_missing() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("permwalk-first");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_head(&install_dir, "abc123");
        let probe = install_dir.join("data.txt");
        fs::write(&probe, "data").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o666)).unwrap();

        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();

        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(
            super::permwalk_stamp_path(&fixture.roots.state_dir, "tool").is_file(),
            "the first walk must leave a stamp so later fresh runs skip"
        );
    }

    #[cfg(unix)]
    #[test]
    fn permwalk_walks_when_head_or_mtime_changes() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("permwalk-invalidate");
        let install_dir = fixture.write_clone("tool");
        fixture.write_git_head(&install_dir, "abc123");
        let probe = install_dir.join("data.txt");
        fs::write(&probe, "data").unwrap();
        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();

        // New HEAD (e.g. an out-of-band pull) invalidates the stamp.
        fixture.write_git_head(&install_dir, "def456");
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o666)).unwrap();
        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o644,
            "a HEAD change must re-run the walk"
        );

        // A root mtime change (new top-level entry) also invalidates it.
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o666)).unwrap();
        fs::write(install_dir.join("new-file"), "x").unwrap();
        super::secure_managed_clone_permissions_cached(
            &fixture.roots.state_dir,
            "tool",
            &install_dir,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&probe).unwrap().permissions().mode() & 0o777,
            0o644,
            "a root mtime change must re-run the walk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_head_direct_resolves_refs_without_spawns() {
        let fixture = Fixture::new("head-resolve");
        let install_dir = fixture.write_clone("tool");
        let git_dir = install_dir.join(".git");

        // Loose ref (the common post-clone layout).
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(git_dir.join("refs/heads/main"), "abc123\n").unwrap();
        assert_eq!(
            super::git_head_direct(&install_dir).as_deref(),
            Some("abc123")
        );

        // Packed ref fallback once the loose ref is gone.
        fs::remove_file(git_dir.join("refs/heads/main")).unwrap();
        fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \nabc123 refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(
            super::git_head_direct(&install_dir).as_deref(),
            Some("abc123")
        );

        // Detached HEAD carries the revision inline.
        fs::write(git_dir.join("HEAD"), "def456\n").unwrap();
        assert_eq!(
            super::git_head_direct(&install_dir).as_deref(),
            Some("def456")
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_head_direct_fails_closed() {
        let fixture = Fixture::new("head-closed");
        // No `.git` at all.
        let bare = fixture.install_dir("bare");
        fs::create_dir_all(&bare).unwrap();
        assert_eq!(super::git_head_direct(&bare), None);

        // Empty HEAD, dangling ref, and path escape all fail closed.
        let install_dir = fixture.write_clone("tool");
        let git_dir = install_dir.join(".git");
        fs::write(git_dir.join("HEAD"), "").unwrap();
        assert_eq!(super::git_head_direct(&install_dir), None);
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(super::git_head_direct(&install_dir), None);
        fs::write(git_dir.join("HEAD"), "ref: ../../escape\n").unwrap();
        assert_eq!(super::git_head_direct(&install_dir), None);
    }

    #[cfg(unix)]
    #[test]
    fn permwalk_stamp_parse_rejects_garbage() {
        assert!(super::permwalk_stamp_parse("").is_none());
        assert!(super::permwalk_stamp_parse("2\nabc\n1\n2\n").is_none());
        assert!(super::permwalk_stamp_parse("1\n\n1\n2\n").is_none());
        assert!(super::permwalk_stamp_parse("1\nabc\nNaN\n2\n").is_none());
        assert!(super::permwalk_stamp_parse("1\nabc\n1\n2\nextra\n").is_none());
        let parsed = super::permwalk_stamp_parse("1\nabc\n1\n2\n").unwrap();
        assert_eq!(parsed.head, "abc");
        assert_eq!(parsed.mtime_secs, 1);
        assert_eq!(parsed.mtime_nanos, 2);
    }

    fn missing_path() -> PathBuf {
        crate::test_support::temp_dir("shdeps-update-repo-absent").join("does-not-exist.gitconfig")
    }

    #[test]
    fn fresh_install_existing_repairs_deleted_bin_link() {
        // The fresh path still runs link reconciliation: a user-deleted public
        // symlink must come back even when the remote stamp is current. Any
        // fast path must preserve this repair behavior.
        let fixture = Fixture::new("fresh-repair");
        let install_dir = fixture.write_clone("tool");
        fixture.write_fresh_stamp("tool");
        manifest::upsert(
            &fixture.manifest_path,
            manifest::ManifestEntry::new(
                "tool",
                crate::method::GITHUB_REPO,
                "tool",
                install_dir.display().to_string(),
            ),
        )
        .unwrap();
        let runner = FakeRunner::default()
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                true,
                "https://github.com/owner/tool.git\n",
            )
            .with_output(
                "git",
                [
                    "-C",
                    &install_dir.display().to_string(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );
        let context = fixture.context(&runner, "apt");
        let first = install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &install_dir,
            "https://github.com/owner/tool",
            &mut crate::hooks::MutationIntent::new(&fixture.roots.state_dir, "tool"),
        )
        .unwrap();
        assert_eq!(first.reason, ItemReason::Fresh);
        fs::remove_file(fixture.roots.bin_dir.join("tool")).unwrap();

        let second = install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &install_dir,
            "https://github.com/owner/tool",
            &mut crate::hooks::MutationIntent::new(&fixture.roots.state_dir, "tool"),
        )
        .unwrap();

        assert_eq!(second.reason, ItemReason::Fresh);
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_dir.join("bin").join("tool")
        );
    }

    /// Real Git isolated from the developer's global and system config, so a
    /// personal `insteadOf`, hook, or localized message catalog cannot change
    /// what these tests observe.
    struct HermeticGit;

    impl Runner for HermeticGit {
        fn exists(&self, command: &str) -> bool {
            command == "git"
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            let mut command = std::process::Command::new(program);
            hermetic_git_env(&mut command);
            command.args(args);
            let output = crate::test_support::run_subprocess(command)?;
            Ok(Output {
                success: output.status.success(),
                timed_out: false,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    fn hermetic_git_env(command: &mut std::process::Command) {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
    }

    fn fixture_git(dir: &Path, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        hermetic_git_env(&mut command);
        command
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-C",
            ])
            .arg(dir)
            .args(args);
        let output = crate::test_support::run_subprocess(command).unwrap();
        assert!(
            output.status.success(),
            "git -C {} {} failed: {}",
            dir.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A bare `main` origin, a seed working copy that publishes to it, and a
    /// shallow managed clone made the way `install_fresh` makes one.
    struct RealCheckout {
        origin: PathBuf,
        seed: PathBuf,
        install_dir: PathBuf,
    }

    impl RealCheckout {
        fn new(fixture: &Fixture, name: &str) -> Self {
            let origin = fixture.roots.home.join("origin.git");
            let seed = fixture.roots.home.join("seed");
            fs::create_dir_all(&origin).unwrap();
            fs::create_dir_all(&seed).unwrap();
            fixture_git(&origin, &["init", "--quiet", "--bare"]);
            fixture_git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            fixture_git(&seed, &["init", "--quiet"]);
            fixture_git(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            fs::write(seed.join("README"), "one\n").unwrap();
            fixture_git(&seed, &["add", "README"]);
            fixture_git(&seed, &["commit", "--quiet", "-m", "one"]);
            let url = format!("file://{}", origin.display());
            fixture_git(&seed, &["remote", "add", "origin", &url]);
            fixture_git(&seed, &["push", "--quiet", "origin", "main"]);
            let install_dir = fixture.install_dir(name);
            fixture_git(
                &fixture.roots.home,
                &[
                    "clone",
                    "--quiet",
                    "--depth",
                    "1",
                    &url,
                    &install_dir.display().to_string(),
                ],
            );
            Self {
                origin,
                seed,
                install_dir,
            }
        }

        fn url(&self) -> String {
            format!("file://{}", self.origin.display())
        }

        fn publish(&self, content: &str, extra: &[&str]) {
            fs::write(self.seed.join("README"), content).unwrap();
            fixture_git(&self.seed, &["add", "README"]);
            let mut commit = vec!["commit", "--quiet", "-m", content.trim()];
            commit.extend_from_slice(extra);
            fixture_git(&self.seed, &commit);
            fixture_git(
                &self.seed,
                &["push", "--quiet", "--force", "origin", "main"],
            );
        }

        fn head(&self) -> String {
            fixture_git(&self.install_dir, &["rev-parse", "HEAD"])
        }
    }

    fn run_existing(fixture: &Fixture, checkout: &RealCheckout) -> crate::update::Item {
        let runner = HermeticGit;
        let context = fixture.context(&runner, "apt");
        install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &checkout.install_dir,
            &checkout.url(),
            &mut crate::hooks::MutationIntent::new(&fixture.roots.state_dir, "tool"),
        )
        .unwrap()
    }

    #[test]
    fn real_fetch_failure_reports_git_cause_not_fast_forward() {
        // The reported incident: the network fetch failed, nothing was
        // fetched, and the clean checkout already matched upstream, yet the
        // warning said "no fast-forward". Hide the origin (an empty directory
        // in its place, so the URL still resolves on every platform).
        let fixture = Fixture::new("real-fetch-failure");
        let checkout = RealCheckout::new(&fixture, "tool");
        fs::rename(&checkout.origin, fixture.roots.home.join("moved.git")).unwrap();
        fs::create_dir_all(&checkout.origin).unwrap();

        let item = run_existing(&fixture, &checkout);

        assert_eq!(item.reason, ItemReason::RepoPullFailed);
        assert!(!item.failed);
        assert!(
            item.detail.starts_with("pull failed (fetch failed: ")
                && item
                    .detail
                    .contains("does not appear to be a git repository"),
            "{}",
            item.detail
        );
        let record = crate::stale_remote::read(&crate::stale_remote::record_path(
            &fixture.roots.state_dir,
            "tool",
        ))
        .expect("a failed refresh must leave a pull-failure record");
        assert_eq!(record.failure.reason, crate::stale_remote::Reason::Fetch);
        assert_eq!((record.since, record.last), (NOW, NOW));
        assert_eq!(
            fixture_git(&checkout.install_dir, &["remote", "get-url", "origin"]),
            checkout.url()
        );
        assert!(
            !stamp::remote_path(&fixture.roots.state_dir, "tool", "repo").exists(),
            "a failed refresh must not refresh the TTL stamp"
        );
    }

    #[test]
    fn real_deleted_upstream_branch_reports_it_not_a_fetch_failure() {
        // The repository renamed its default branch: origin is reachable but
        // the shallow clone's single-branch fetch asks for a ref that is
        // gone. That needs a new clone, not a network check.
        let fixture = Fixture::new("real-upstream-gone");
        let checkout = RealCheckout::new(&fixture, "tool");
        fixture_git(
            &checkout.origin,
            &["branch", "--quiet", "-m", "main", "trunk"],
        );

        let item = run_existing(&fixture, &checkout);

        assert_eq!(item.reason, ItemReason::RepoPullFailed);
        assert!(!item.failed);
        assert!(
            item.detail
                .starts_with("pull failed (upstream branch deleted: ")
                && item.detail.contains("refs/heads/main"),
            "{}",
            item.detail
        );
        let record = crate::stale_remote::read(&crate::stale_remote::record_path(
            &fixture.roots.state_dir,
            "tool",
        ))
        .expect("a failed refresh must leave a pull-failure record");
        assert_eq!(
            record.failure.reason,
            crate::stale_remote::Reason::UpstreamGone
        );
    }

    #[test]
    fn real_rewritten_upstream_reports_divergence() {
        let fixture = Fixture::new("real-diverged");
        let checkout = RealCheckout::new(&fixture, "tool");
        checkout.publish("rewritten\n", &["--amend"]);

        let item = run_existing(&fixture, &checkout);

        assert_eq!(item.reason, ItemReason::RepoPullFailed);
        assert_eq!(item.detail, "pull failed (diverged from origin)");
    }

    #[test]
    fn real_conflicting_local_edit_reports_dirty_tree() {
        let fixture = Fixture::new("real-dirty");
        let checkout = RealCheckout::new(&fixture, "tool");
        checkout.publish("two\n", &[]);
        fs::write(checkout.install_dir.join("README"), "local edit\n").unwrap();

        let item = run_existing(&fixture, &checkout);

        assert_eq!(item.reason, ItemReason::RepoPullFailed);
        assert_eq!(item.detail, "pull failed (dirty working tree)");
    }

    #[test]
    fn real_successful_refresh_fast_forwards_and_clears_the_record() {
        let fixture = Fixture::new("real-success");
        let checkout = RealCheckout::new(&fixture, "tool");
        let before = checkout.head();
        checkout.publish("two\n", &[]);
        crate::stale_remote::record_failure(
            &fixture.roots.state_dir,
            "tool",
            &crate::stale_remote::Failure::new(crate::stale_remote::Reason::Fetch, "boom"),
            NOW - 60,
        )
        .unwrap();

        let item = run_existing(&fixture, &checkout);

        assert!(item.changed, "{item:?}");
        assert_ne!(checkout.head(), before);
        assert_eq!(
            fs::read_to_string(checkout.install_dir.join("README")).unwrap(),
            "two\n"
        );
        assert!(
            !crate::stale_remote::record_path(&fixture.roots.state_dir, "tool").exists(),
            "a successful refresh must remove the pull-failure record"
        );
        assert!(stamp::remote_path(&fixture.roots.state_dir, "tool", "repo").exists());
    }
}
