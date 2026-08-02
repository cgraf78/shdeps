//! `github:release` update execution.
//!
//! This module keeps the network/update orchestration separate from the
//! filesystem installer helpers. The flow mirrors Bash's public contract: TTL
//! fast path, release metadata fetch, host asset selection, asset download,
//! install, stamp, and manifest repair.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::Result;
use crate::config::Entry;
use crate::github;
use crate::github_release;
use crate::github_release_install;
use crate::http::Client;
use crate::jobs;
use crate::manifest::{self, ManifestEntry};
use crate::method;
use crate::platform::RuntimeEnv;
use crate::process::{self, Runner};
use crate::release_asset::AssetKind;
use crate::runtime::{Env, Roots};
use crate::stamp;
use crate::update::{
    self, Context, Item, ItemReason, Options, Progress, detail_with_action, verbose_enabled,
};

pub(crate) fn install_with_prefetch(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    options: Options,
    prefetch: &Prefetch,
) -> Result<Item> {
    let bin_path = context.roots.bin_dir.join(&entry.cmd);
    let request = ReleaseRequest {
        name: &entry.name,
        cmd: &entry.cmd,
        repo: &entry.name,
        public_bin: &bin_path,
    };
    let request_context = RequestContext {
        roots: context.roots,
        runtime_env: context.env,
        env_vars: context.env_vars,
        runner: context.runner,
        client: context.client,
        options,
        prefetch,
    };
    let outcome = install_request(&request, &request_context)?;
    if !outcome.failed {
        // Ordering matters: write the manifest record BEFORE refreshing the
        // TTL stamp. If a crash/SIGINT lands between the two writes, the next
        // shdeps run must see "stamp older than manifest" (re-evaluate) rather
        // than "fresh stamp pointing at a dep the manifest does not know" —
        // the latter shape lets the TTL fast path skip the dep while
        // `manifest.get(name)` keeps returning `None`. The stamp is the
        // freshness gate; the manifest is the durable record. Always write
        // the durable record first.
        write_manifest(entry, context)?;
        if outcome.stamp {
            let stamp_path = stamp::remote_path(&context.roots.state_dir, &entry.name, "release");
            stamp::remote_touch(&stamp_path, options.now)?;
        }
    }

    Ok(match (outcome.failed, outcome.changed) {
        (true, _) => Item::failed(
            entry.name.clone(),
            ItemReason::InstallFailed,
            outcome.detail,
        ),
        (false, true) => Item::changed(entry.name.clone(), ItemReason::Installed, outcome.detail),
        (false, false) => Item::current(entry.name.clone(), ItemReason::Installed, outcome.detail),
    })
}

#[derive(Debug, Default)]
pub(crate) struct Prefetch {
    releases: BTreeMap<String, Vec<github::Release>>,
    versions: BTreeMap<String, String>,
    current: BTreeSet<String>,
    token: OnceLock<Option<String>>,
}

impl Prefetch {
    fn releases(&self, repo: &str) -> Option<&[github::Release]> {
        self.releases.get(repo).map(Vec::as_slice)
    }

    fn version(&self, name: &str) -> Option<&str> {
        self.versions.get(name).map(String::as_str)
    }

    fn is_current(&self, name: &str) -> bool {
        self.current.contains(name)
    }

    fn token<'a>(&'a self, env: &impl Env, runner: &impl Runner) -> Option<&'a str> {
        self.token
            .get_or_init(|| github::token(env, runner))
            .as_deref()
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    cmd: String,
    repo: String,
    public_bin: PathBuf,
}

pub(crate) fn prefetch<R>(
    entries: &[&Entry],
    context: &Context<'_, R>,
    options: Options,
    progress: &mut dyn Progress,
) -> Result<Prefetch>
where
    R: Runner + Sync,
{
    let candidates = prefetch_candidates(entries, context.roots, options);
    if candidates.is_empty() {
        return Ok(Prefetch::default());
    }

    let env = EnvVars {
        vars: context.env_vars,
        runtime: context.env,
    };
    let mut prefetch = Prefetch {
        releases: BTreeMap::new(),
        versions: BTreeMap::new(),
        current: BTreeSet::new(),
        token: OnceLock::new(),
    };
    let jobs = jobs::github_max(context.env_vars);
    if jobs <= 1 || candidates.len() <= 1 {
        for candidate in &candidates {
            if let Some(releases) = cached_releases(&candidate.repo, context.roots, options) {
                prefetch.releases.insert(candidate.repo.clone(), releases);
            }
        }
        // The shared lazy token remains available to the sequential install
        // path. It is resolved only if the public redirect cannot prove an
        // installed release current, then reused for every API request.
        return Ok(prefetch);
    }

    let version_candidates = candidates
        .iter()
        .filter(|candidate| process::executable_path(&candidate.public_bin))
        .collect::<Vec<_>>();
    if !version_candidates.is_empty() {
        // Version probes are local-only and cached only when they succeed; a
        // missing or odd tool gets the old serial second chance during install.
        for fetched in jobs::parallel_map_with_progress(
            &version_candidates,
            jobs,
            |candidate| {
                process::dep_version(context.runner, &candidate.cmd)
                    .map(|version| (candidate.name.clone(), version))
            },
            |done| {
                progress.phase(update::running_phase(
                    update::PHASE_GITHUB_RELEASE_VERSIONS,
                    done,
                    version_candidates.len(),
                ))
            },
        )? {
            let Some((name, version)) = fetched else {
                continue;
            };
            prefetch.versions.insert(name, version);
        }
    }

    // Each remote worker first asks GitHub's public redirect for the latest
    // tag. When that tag matches the installed command version, no REST API
    // request is needed. New installs, changed tags, private repositories, and
    // malformed redirects fall through to the existing authoritative release
    // JSON request in the same worker. One result per candidate keeps progress
    // totals stable even though a fallback worker can perform two HTTP calls.
    let versions = &prefetch.versions;
    let token = &prefetch.token;
    for probe in jobs::parallel_map_with_progress(
        &candidates,
        jobs,
        |candidate| {
            if let Some(releases) = cached_releases(&candidate.repo, context.roots, options) {
                return Some(RemoteProbe::Releases(candidate.repo.clone(), releases));
            }

            if !options.reinstall
                && versions.get(&candidate.name).is_some_and(|version| {
                    github::latest_release_matches(&candidate.repo, version, context.client)
                        .unwrap_or(false)
                })
            {
                return Some(RemoteProbe::Current(candidate.name.clone()));
            }

            {
                let token = token
                    .get_or_init(|| github::token(&env, context.runner))
                    .as_deref();
                github::fetch_releases_with_token(&candidate.repo, context.client, token).ok()
            }
            .map(|releases| RemoteProbe::Releases(candidate.repo.clone(), releases))
        },
        |done| {
            progress.phase(update::running_phase(
                update::PHASE_GITHUB_RELEASE_METADATA,
                done,
                candidates.len(),
            ))
        },
    )?
    .into_iter()
    .flatten()
    {
        match probe {
            RemoteProbe::Current(name) => {
                prefetch.current.insert(name);
            }
            RemoteProbe::Releases(repo, releases) => {
                prefetch.releases.insert(repo, releases);
            }
        }
    }

    Ok(prefetch)
}

enum RemoteProbe {
    Current(String),
    Releases(String, Vec<github::Release>),
}

pub(crate) fn prefetch_progress_total(
    entries: &[&Entry],
    roots: &Roots,
    options: Options,
) -> usize {
    prefetch_candidates(entries, roots, options).len()
}

fn prefetch_candidates(entries: &[&Entry], roots: &Roots, options: Options) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let public_bin = roots.bin_dir.join(&entry.cmd);
        let stamp_path = stamp::remote_path(&roots.state_dir, &entry.name, "release");
        if process::executable_path(&public_bin)
            && (stamp::remote_fresh(&stamp_path, options.freshness())
                || checked_this_run(&stamp_path, options))
        {
            continue;
        }
        // Duplicate repo rows are unusual but legal enough that the optimizer
        // should be defensive. Fetching once preserves install behavior while
        // preventing the prefetch phase from multiplying GitHub API pressure.
        if seen.insert(entry.name.clone()) {
            candidates.push(Candidate {
                name: entry.name.clone(),
                cmd: entry.cmd.clone(),
                repo: entry.name.clone(),
                public_bin,
            });
        }
    }
    candidates
}

pub(crate) struct ReleaseRequest<'a> {
    pub(crate) name: &'a str,
    pub(crate) cmd: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) public_bin: &'a Path,
}

pub(crate) struct RequestContext<'a, R: Runner> {
    pub(crate) roots: &'a Roots,
    pub(crate) runtime_env: &'a RuntimeEnv,
    pub(crate) env_vars: &'a BTreeMap<String, String>,
    pub(crate) runner: &'a R,
    pub(crate) client: &'a dyn Client,
    pub(crate) options: Options,
    pub(crate) prefetch: &'a Prefetch,
}

pub(crate) struct ReleaseOutcome {
    pub(crate) changed: bool,
    pub(crate) failed: bool,
    pub(crate) detail: String,
    /// True when the caller should refresh the TTL stamp after writing the
    /// manifest record. Set to `true` only for paths that confirmed the
    /// remote check (either a fresh install or a `--force`-triggered
    /// re-check that found the latest tag already installed). The
    /// TTL-fast-path ("fresh") and all failure paths leave this `false` so
    /// the stamp remains exactly as observed.
    pub(crate) stamp: bool,
}

pub(crate) fn install_request(
    request: &ReleaseRequest<'_>,
    context: &RequestContext<'_, impl Runner>,
) -> Result<ReleaseOutcome> {
    let stamp_path = stamp::remote_path(&context.roots.state_dir, request.name, "release");
    if process::executable_path(request.public_bin)
        && (stamp::remote_fresh(&stamp_path, context.options.freshness())
            || checked_this_run(&stamp_path, context.options))
    {
        // A prior phase can verify this release through the public redirect in
        // the same ordinary update. Forced and reinstall runs deliberately do
        // not share persisted state: a prior invocation can write the same
        // second-level timestamp, which is indistinguishable on disk from an
        // in-run check and can otherwise mask stale release metadata.
        // Bash relinks extras even on the TTL fast path. That idempotent repair
        // matters when a user prunes a completion/manpage symlink by hand while
        // keeping the binary; a fresh stamp should skip the network, not leave
        // related shell integration broken until the next forced reinstall.
        let install_dir = context.roots.install_dir.join(request.name);
        crate::extras::link(
            &context.roots.state_dir,
            &context.roots.install_dir,
            request.name,
            &install_dir,
        )?;
        let detail = if verbose_enabled(context.options, context.env_vars) {
            process::dep_version(context.runner, request.cmd).unwrap_or_else(|| "fresh".to_owned())
        } else {
            "fresh".to_owned()
        };
        return Ok(ReleaseOutcome {
            changed: false,
            failed: false,
            detail,
            // The TTL stamp is already fresh in this branch; nothing to
            // refresh.
            stamp: false,
        });
    }

    let current_version = context
        .prefetch
        .version(request.name)
        .map(ToOwned::to_owned)
        .or_else(|| {
            process::executable_path(request.public_bin)
                .then(|| process::dep_version(context.runner, request.cmd))
                .flatten()
        });

    let redirect_confirms_current = context.prefetch.is_current(request.name)
        || (!context.options.reinstall
            && !context.prefetch.releases.contains_key(request.repo)
            && current_version.as_deref().is_some_and(|current| {
                github::latest_release_matches(request.repo, current, context.client)
                    .unwrap_or(false)
            }));
    if redirect_confirms_current {
        link_existing_extras(context.roots, request.name)?;
        return Ok(ReleaseOutcome {
            changed: false,
            failed: false,
            detail: current_version.unwrap_or_else(|| "current".to_owned()),
            // The public redirect proved the installed tag is still GitHub's
            // latest stable release, so this is a successful remote check.
            stamp: true,
        });
    }

    let env = EnvVars {
        vars: context.env_vars,
        runtime: context.runtime_env,
    };
    let fetched_releases;
    let releases = if let Some(releases) = context.prefetch.releases(request.repo) {
        releases
    } else {
        fetched_releases = match cached_releases(request.repo, context.roots, context.options)
            .map(Ok)
            .unwrap_or_else(|| {
                fetch_releases_with_prefetch_token(
                    request.repo,
                    &env,
                    context.runner,
                    context.client,
                    context.prefetch,
                )
            }) {
            Ok(releases) => releases,
            Err(error) => {
                let failure = github::fetch_failure(&error);
                if process::executable_path(request.public_bin) && !context.options.reinstall {
                    // GitHub metadata is a freshness check, not proof that an
                    // already-installed binary disappeared. Fleet updates can
                    // exhaust unauthenticated API quota long before anything
                    // is actually stale, so preserve the working tool and try
                    // again on the next run instead of turning a transient API
                    // failure into a broken `dot update`. Rate-limited checks
                    // say so in the detail so stale "current" rows are
                    // distinguishable from verified ones.
                    let detail = match (current_version, failure) {
                        (Some(version), github::FetchFailure::RateLimited) => {
                            format!("{version} (update check rate-limited)")
                        }
                        (Some(version), _) => version,
                        (None, github::FetchFailure::RateLimited) => {
                            "update check rate-limited".to_owned()
                        }
                        (None, _) => "release metadata unavailable".to_owned(),
                    };
                    link_existing_extras(context.roots, request.name)?;
                    return Ok(ReleaseOutcome {
                        changed: false,
                        failed: false,
                        detail,
                        // Transient metadata failure: preserve the working
                        // binary but do NOT refresh the TTL stamp so the
                        // next run will retry the network check.
                        stamp: false,
                    });
                }
                return Ok(failed(match failure {
                    github::FetchFailure::RateLimited => {
                        "GitHub API rate limit exceeded; retry after the window resets or set GH_TOKEN"
                    }
                    github::FetchFailure::NotFound => {
                        "no GitHub release metadata (repo missing, private, or unreleased)"
                    }
                    github::FetchFailure::Other => "release metadata fetch failed",
                }));
            }
        };
        &fetched_releases
    };
    if let Some(latest) = github_release::latest_stable(releases) {
        if !context.options.reinstall
            && current_version
                .as_deref()
                .is_some_and(|current| github::installed_matches_tag(current, &latest.tag))
        {
            // `--force` deliberately bypasses the TTL so users can ask GitHub
            // "is there anything newer?" immediately. It must not imply a
            // reinstall loop: release downloads are the most expensive shdeps
            // path, and post hooks should only run when the dependency really
            // changed. Bash compared the probed command version to the latest
            // release tag before asset selection, so keep that no-op path even
            // if the release has no asset we would install from scratch today.
            link_existing_extras(context.roots, request.name)?;
            return Ok(ReleaseOutcome {
                changed: false,
                failed: false,
                detail: current_version.unwrap_or_else(|| latest.tag.clone()),
                // The remote was checked and confirms the installed tag is
                // current. Refresh the stamp — but only after the caller
                // has upserted the manifest record, so that a crash
                // between manifest and stamp can never leave a fresh
                // stamp paired with a stale or missing manifest entry.
                stamp: true,
            });
        }
    }
    let Some(selection) =
        github_release::select(request.cmd, releases, context.runtime_env, context.runner)
    else {
        return Ok(failed("no matching release asset"));
    };
    let asset_token = github_token(&env, context);
    let bytes = match github::download_asset(
        context.client,
        &selection.url,
        selection.api_url.as_deref(),
        asset_token.as_deref(),
    ) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(failed("release asset download failed")),
    };
    // Best-effort integrity verification: when the release publishes a
    // recognized checksum asset, verify the downloaded bytes against the
    // expected digest before letting the binary touch the install dir.
    // Verification binds to the asset filename so a multi-asset release cannot
    // accidentally accept a digest computed for a different platform's binary.
    // When no checksum asset exists, installation continues unverified — many
    // older third-party releases do not ship one and breaking those installs
    // would be a regression.
    // The shdeps self-update path (`release_stage`) uses a stricter
    // contract that requires the checksum and rejects on download failure;
    // the third-party path here intentionally diverges in failure mode.
    if !selection.checksum_assets.is_empty() {
        let mut verified = false;
        for checksum_asset in &selection.checksum_assets {
            let checksum_bytes = match github::download_asset(
                context.client,
                &checksum_asset.url,
                checksum_asset.api_url.as_deref(),
                asset_token.as_deref(),
            ) {
                Ok(checksum_bytes) => checksum_bytes,
                Err(_) => {
                    // The checksum asset was advertised in the release JSON
                    // but not retrievable. No install ran (we are NOT going
                    // to land an unverified binary), but we also do not
                    // refresh the TTL stamp so the next run retries the
                    // verification.
                    //
                    // Failure-mode: report as `failed: true` rather than the
                    // earlier soft `failed: false`. The earlier shape let
                    // the caller's unconditional `write_manifest` run on
                    // the success path, recording a manifest row pointing
                    // at a `bin_dir/cmd` that does not exist on disk for
                    // first-time installs. Treating the unavailable
                    // checksum as a real failure keeps the manifest from
                    // advertising a binary the user does not have.
                    return Ok(failed(&format!("{}: checksum unavailable", selection.tag)));
                }
            };
            let checksum_text = String::from_utf8_lossy(&checksum_bytes);
            if crate::checksum::verify_any(&checksum_text, &selection.asset_name, &bytes) {
                verified = true;
                break;
            }
            if crate::checksum::has_named_checksum(&checksum_text, &selection.asset_name) {
                eprintln!(
                    "shdeps: checksum mismatch: asset={} bytes={} sha256={} checksum={}",
                    selection.asset_name,
                    bytes.len(),
                    crate::checksum::sha256_hex(&bytes),
                    checksum_asset.url.rsplit('/').next().unwrap_or("checksum")
                );
                return Ok(failed("release asset checksum mismatch"));
            }
        }
        if !verified {
            eprintln!(
                "shdeps: checksum mismatch: asset={} bytes={} sha256={} checksum has no named entry",
                selection.asset_name,
                bytes.len(),
                crate::checksum::sha256_hex(&bytes)
            );
            return Ok(failed("release asset checksum mismatch"));
        }
    }
    let Some(asset_kind) = crate::release_asset::install_kind(&selection.url) else {
        return Ok(failed("release asset type is not implemented yet"));
    };
    match asset_kind {
        AssetKind::Plain => {
            github_release_install::install_plain_to(request.public_bin, &bytes)?;
            clear_archive_bin_links(context, request);
        }
        AssetKind::Gz => {
            github_release_install::install_gz_to(request.public_bin, &bytes)?;
            clear_archive_bin_links(context, request);
        }
        AssetKind::Bz2 => {
            github_release_install::install_bz2_to(request.public_bin, &bytes)?;
            clear_archive_bin_links(context, request);
        }
        AssetKind::Xz => {
            github_release_install::install_xz_to(request.public_bin, &bytes)?;
            clear_archive_bin_links(context, request);
        }
        AssetKind::Zst => {
            github_release_install::install_zst_to(request.public_bin, &bytes)?;
            clear_archive_bin_links(context, request);
        }
        AssetKind::TarGz => {
            github_release_install::install_tar_gz_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::Tar => {
            github_release_install::install_tar_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarBz2 => {
            github_release_install::install_tar_bz2_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarZst => {
            github_release_install::install_tar_zst_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::TarXz => {
            github_release_install::install_tar_xz_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
        AssetKind::Zip => {
            github_release_install::install_zip_to(
                &context.roots.state_dir,
                &context.roots.install_dir,
                request.public_bin,
                request.name,
                request.cmd,
                &bytes,
            )?;
        }
    }
    // The TTL stamp is intentionally NOT refreshed here. The caller writes
    // the manifest record first and then touches the stamp, so a crash in
    // between leaves the dep "needs work" rather than "fresh but unknown".
    // See the matching comment in `install_with_prefetch`.
    let _ = stamp_path;

    let detail = if verbose_enabled(context.options, context.env_vars) {
        match current_version {
            Some(current) if github::installed_matches_tag(&current, &selection.tag) => {
                detail_with_action("reinstalled", selection.tag.clone())
            }
            Some(current) => format!("updated -- {current} -> {}", selection.tag),
            None => detail_with_action("added", selection.tag.clone()),
        }
    } else {
        selection.tag.clone()
    };

    Ok(ReleaseOutcome {
        changed: true,
        failed: false,
        detail,
        stamp: true,
    })
}

fn clear_archive_bin_links(
    context: &RequestContext<'_, impl Runner>,
    request: &ReleaseRequest<'_>,
) {
    let _ = github_release_install::clear_archive_bin_links(
        &context.roots.state_dir,
        request.name,
        request.public_bin,
    );
}

fn cached_releases(repo: &str, roots: &Roots, options: Options) -> Option<Vec<github::Release>> {
    if options.force || options.reinstall {
        // `remote_checked_at` is persisted at second resolution, so it cannot
        // prove that a stamp came from this invocation. Force is an explicit
        // request to refresh the complete release decision, including assets
        // and checksums, rather than merely bypassing the normal TTL.
        return None;
    }

    let stamp_path = stamp::remote_path(&roots.state_dir, repo, method::GITHUB);
    if !stamp::remote_fresh(&stamp_path, options.freshness())
        && !checked_this_run(&stamp_path, options)
    {
        return None;
    }

    github::read_cached_releases(&roots.state_dir, repo)
}

/// Returns whether a non-forced update can reuse a remote fact written during
/// its current invocation.
///
/// Stamps contain only seconds, not a run identifier. A forced/reinstall run
/// must therefore never treat an equal timestamp as in-run proof: the stamp
/// might instead belong to a just-finished prior invocation with stale assets.
fn checked_this_run(path: &Path, options: Options) -> bool {
    !options.force && !options.reinstall && stamp::remote_checked_at(path, options.now)
}

fn fetch_releases_with_prefetch_token(
    repo: &str,
    env: &impl Env,
    runner: &impl Runner,
    client: &dyn Client,
    prefetch: &Prefetch,
) -> Result<Vec<github::Release>> {
    github::fetch_releases_with_token(repo, client, prefetch.token(env, runner))
}

fn github_token(env: &impl Env, context: &RequestContext<'_, impl Runner>) -> Option<String> {
    context
        .prefetch
        .token(env, context.runner)
        .map(ToOwned::to_owned)
}

fn link_existing_extras(roots: &Roots, name: &str) -> Result<()> {
    let install_dir = roots.install_dir.join(name);
    crate::extras::link(&roots.state_dir, &roots.install_dir, name, &install_dir)?;
    Ok(())
}

fn write_manifest(entry: &Entry, context: &Context<'_, impl Runner>) -> Result<()> {
    manifest::upsert(
        context.manifest_path,
        ManifestEntry::new(
            &entry.name,
            method::GITHUB_RELEASE,
            &entry.cmd,
            context.roots.bin_dir.join(&entry.cmd).display().to_string(),
        ),
    )
}

fn failed(detail: &str) -> ReleaseOutcome {
    ReleaseOutcome {
        changed: false,
        failed: true,
        detail: detail.to_owned(),
        // A failed run must not refresh the TTL stamp; otherwise the next
        // run's fast path could mask the persistent failure.
        stamp: false,
    }
}

struct EnvVars<'a> {
    vars: &'a std::collections::BTreeMap<String, String>,
    runtime: &'a RuntimeEnv,
}

impl Env for EnvVars<'_> {
    fn var_os(&self, name: &str) -> Option<OsString> {
        self.vars.get(name).map(OsString::from)
    }

    fn command_output(&self, command: &str, args: &[&str]) -> Option<String> {
        match (command, args) {
            ("uname", ["-s"]) => Some(self.runtime.platform().to_owned()),
            ("hostname", ["-s"] | []) => Some(self.runtime.host().to_owned()),
            _ => None,
        }
    }

    fn read_to_string(&self, _path: &std::path::Path) -> Option<String> {
        None
    }
}
