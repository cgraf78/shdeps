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
        secure_managed_clone_permissions_cached(
            &context.roots.state_dir,
            &entry.name,
            install_dir,
        )?;
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
        let _ = write_permwalk_stamp(&context.roots.state_dir, &entry.name, install_dir);
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
    let _ = write_permwalk_stamp(&context.roots.state_dir, &entry.name, install_dir);
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
        if refname.is_empty() || refname.contains("..") {
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
    sync_ssh_push_url_with_configs(
        runner,
        install_dir,
        &user_git_config_paths(),
        Path::new("/etc/gitconfig"),
        std::env::var_os("HOME").map(PathBuf::from),
        &git_config_env_snapshot(),
    );
}

fn sync_ssh_push_url_with_configs(
    runner: &impl Runner,
    install_dir: &Path,
    user_configs: &[PathBuf],
    system_config: &Path,
    home: Option<PathBuf>,
    env: &GitConfigEnv,
) {
    if !env.hard_override
        && push_url_sync_redundant_with_configs(
            install_dir,
            user_configs,
            system_config,
            home.as_deref(),
            &env.count_rewrites,
        )
    {
        return;
    }
    let Some(origin) = remote_origin(runner, install_dir) else {
        return;
    };
    if let Some(fallback) = expected_push_url(&origin) {
        set_push_url(runner, install_dir, &fallback);
    }
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
/// Every no-op update runs this per repo dependency (two git spawns each:
/// `get-url` + `set-url --push`). A converged clone already carries the
/// expected push URL in `.git/config`, so read that file directly and skip
/// both spawns. Fail-closed: any ambiguity returns false and the git-based
/// sync runs unchanged, so the worst case is today's cost, never a wrong
/// skip.
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

        fn context<'a>(
            &'a self,
            runner: &'a FakeRunner,
            pkg_mgr: &'a str,
        ) -> Context<'a, FakeRunner> {
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
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        sync_ssh_push_url(&runner, &install_dir);

        assert_eq!(
            runner.git_calls(),
            vec![
                git_args(&install_dir, &["remote", "get-url", "origin"]),
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
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        sync_ssh_push_url(&runner, &install_dir);

        assert_eq!(runner.git_calls().len(), 2);
        assert_eq!(
            runner.git_calls()[1],
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
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:owner/tool.git",
                ],
                true,
                "",
            );

        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &GitConfigEnv::default(),
        );

        assert_eq!(runner.git_calls().len(), 2);
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
        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &outcome_changing,
        );
        assert_eq!(
            runner.git_calls().len(),
            2,
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
        sync_ssh_push_url_with_configs(
            &runner,
            &install_dir,
            &[],
            &missing_path(),
            None,
            &overridden,
        );
        assert_eq!(
            runner.git_calls().len(),
            2,
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
        )
        .unwrap();
        assert_eq!(first.reason, ItemReason::Fresh);
        fs::remove_file(fixture.roots.bin_dir.join("tool")).unwrap();

        let second = install_existing(
            &fixture.entry("tool"),
            &context,
            Fixture::options(),
            &install_dir,
        )
        .unwrap();

        assert_eq!(second.reason, ItemReason::Fresh);
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_dir.join("bin").join("tool")
        );
    }
}
