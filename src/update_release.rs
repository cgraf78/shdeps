//! `github:release` update execution.
//!
//! This module keeps the network/update orchestration separate from the
//! filesystem installer helpers. The flow mirrors Bash's public contract: TTL
//! fast path, release metadata fetch, host asset selection, asset download,
//! install, stamp, and manifest repair.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
use crate::runtime::{Env, Roots};
use crate::stamp;
use crate::tool_version;
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
    token: Option<String>,
    token_resolved: bool,
}

impl Prefetch {
    fn releases(&self, repo: &str) -> Option<&[github::Release]> {
        self.releases.get(repo).map(Vec::as_slice)
    }

    fn version(&self, name: &str) -> Option<&str> {
        self.versions.get(name).map(String::as_str)
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
    let token = github::token(&env, context.runner);
    let mut prefetch = Prefetch {
        releases: BTreeMap::new(),
        versions: BTreeMap::new(),
        token: token.clone(),
        token_resolved: true,
    };
    let jobs = jobs::github_max(context.env_vars);
    if jobs <= 1 || candidates.len() <= 1 {
        // Still cache the token in the sequential path. The old Bash
        // implementation naturally held this in shell state, but the Rust port
        // otherwise probes `gh auth token` once per metadata/asset request.
        // Avoiding that subprocess fan-out is a warm-path performance win and
        // also keeps CI logs quieter when GitHub credentials are present.
        return Ok(prefetch);
    }

    // Prefetch only read-only release facts. Installation order is part of the
    // public behavior because post hooks and method-transition cleanup are
    // observable, but metadata reads are safe to overlap because they have no
    // local mutation. A failed metadata prefetch is deliberately not cached so
    // the normal per-dependency path can retry and report the same failure it
    // would have reported without this optimization.
    for (repo, releases) in jobs::parallel_map_with_progress(
        &candidates,
        jobs,
        |candidate| {
            cached_releases(&candidate.repo, context.roots, options)
                .or_else(|| {
                    github::fetch_releases_with_token(
                        &candidate.repo,
                        context.client,
                        token.as_deref(),
                    )
                    .ok()
                })
                .map(|releases| (candidate.repo.clone(), releases))
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
        prefetch.releases.insert(repo, releases);
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

    Ok(prefetch)
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
            && stamp::remote_fresh(&stamp_path, options.freshness())
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
        && stamp::remote_fresh(&stamp_path, context.options.freshness())
    {
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
            Err(_) => {
                if process::executable_path(request.public_bin) && !context.options.reinstall {
                    // GitHub metadata is a freshness check, not proof that an
                    // already-installed binary disappeared. Fleet updates can
                    // exhaust unauthenticated API quota long before anything
                    // is actually stale, so preserve the working tool and try
                    // again on the next run instead of turning a transient API
                    // failure into a broken `dot update`.
                    let detail = current_version
                        .unwrap_or_else(|| "release metadata unavailable".to_owned());
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
                return Ok(failed("release metadata fetch failed"));
            }
        };
        &fetched_releases
    };
    if let Some(latest) = github_release::latest_stable(releases) {
        if !context.options.reinstall
            && current_version
                .as_deref()
                .is_some_and(|current| installed_matches_tag(current, &latest.tag))
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
    // sibling `<asset>.sha256`, verify the downloaded bytes against the
    // expected digest before letting the binary touch the install dir.
    // The verification binds to the asset filename (via `checksum::verify`)
    // so a multi-asset release cannot accidentally accept a digest computed
    // for a different platform's binary. When no checksum asset exists,
    // installation continues unverified — many older third-party releases
    // do not ship one and breaking those installs would be a regression.
    // The shdeps self-update path (`release_stage`) uses a stricter
    // contract that requires the checksum and rejects on download failure;
    // the third-party path here intentionally diverges in failure mode.
    if let Some(checksum_url) = selection.checksum_url.as_deref() {
        match github::download_asset(
            context.client,
            checksum_url,
            selection.checksum_api_url.as_deref(),
            asset_token.as_deref(),
        ) {
            Ok(checksum_bytes) => {
                let checksum_text = String::from_utf8_lossy(&checksum_bytes);
                if !crate::checksum::verify(&checksum_text, &selection.asset_name, &bytes) {
                    return Ok(failed("release asset checksum mismatch"));
                }
            }
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
        }
    }
    match asset_kind(&selection.url) {
        AssetKind::Plain => {
            github_release_install::install_plain_to(request.public_bin, &bytes)?;
        }
        AssetKind::Gz => {
            github_release_install::install_gz_to(request.public_bin, &bytes)?;
        }
        AssetKind::Bz2 => {
            github_release_install::install_bz2_to(request.public_bin, &bytes)?;
        }
        AssetKind::Zst => {
            github_release_install::install_zst_to(request.public_bin, &bytes)?;
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
        AssetKind::Unsupported => {
            return Ok(failed("release asset type is not implemented yet"));
        }
    }
    // The TTL stamp is intentionally NOT refreshed here. The caller writes
    // the manifest record first and then touches the stamp, so a crash in
    // between leaves the dep "needs work" rather than "fresh but unknown".
    // See the matching comment in `install_with_prefetch`.
    let _ = stamp_path;

    let detail = if verbose_enabled(context.options, context.env_vars) {
        match current_version {
            Some(current) if installed_matches_tag(&current, &selection.tag) => {
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

fn cached_releases(repo: &str, roots: &Roots, options: Options) -> Option<Vec<github::Release>> {
    let stamp_path = stamp::remote_path(&roots.state_dir, repo, method::GITHUB);
    if !stamp::remote_fresh(&stamp_path, options.freshness())
        && !stamp::remote_checked_at(&stamp_path, options.now)
    {
        return None;
    }

    github::read_cached_releases(&roots.state_dir, repo)
}

fn fetch_releases_with_prefetch_token(
    repo: &str,
    env: &impl Env,
    runner: &impl Runner,
    client: &dyn Client,
    prefetch: &Prefetch,
) -> Result<Vec<github::Release>> {
    if prefetch.token_resolved {
        github::fetch_releases_with_token(repo, client, prefetch.token.as_deref())
    } else {
        github::fetch_releases(repo, env, runner, client)
    }
}

fn github_token(env: &impl Env, context: &RequestContext<'_, impl Runner>) -> Option<String> {
    if context.prefetch.token_resolved {
        return context.prefetch.token.clone();
    }
    github::token(env, context.runner)
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

fn installed_matches_tag(installed: &str, tag: &str) -> bool {
    comparable_versions(tag)
        .into_iter()
        .any(|candidate| candidate == installed)
}

fn comparable_versions(tag: &str) -> Vec<String> {
    let tag = tag.trim();
    let mut versions = Vec::new();

    // Keep exact-ish comparisons first for projects that report their tag
    // verbatim. Then add the de-prefixed and dotted forms that cover common
    // GitHub tags such as `v1.2.3`, `rust-v0.133.0`, and `release-2026.5.15`.
    push_unique(&mut versions, tag);
    push_unique(&mut versions, tag.strip_prefix('v').unwrap_or(tag));

    if let Some(dotted) = tool_version::extract(&[tag], "release-tag") {
        push_unique(&mut versions, &dotted);
    }

    versions
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if value.is_empty() || values.iter().any(|existing| existing == value) {
        return;
    }
    values.push(value.to_owned());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Plain,
    Gz,
    Bz2,
    Zst,
    TarGz,
    TarBz2,
    TarZst,
    TarXz,
    Zip,
    Unsupported,
}

fn asset_kind(url: &str) -> AssetKind {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return AssetKind::TarGz;
    }
    if lower.ends_with(".tar.bz2") {
        return AssetKind::TarBz2;
    }
    if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        return AssetKind::TarZst;
    }
    if lower.ends_with(".tar.xz") {
        return AssetKind::TarXz;
    }
    // The Bash reference supports xz only as a tar archive. Treating a bare
    // `.xz` asset as a plain executable would publish compressed bytes into
    // `SHDEPS_BIN`, so keep this explicitly unsupported until single-file xz
    // installs are added to both implementations.
    if lower.ends_with(".xz") {
        return AssetKind::Unsupported;
    }
    if lower.ends_with(".gz") {
        return AssetKind::Gz;
    }
    if lower.ends_with(".bz2") {
        return AssetKind::Bz2;
    }
    if lower.ends_with(".zst") {
        return AssetKind::Zst;
    }
    if lower.ends_with(".zip") {
        return AssetKind::Zip;
    }
    AssetKind::Plain
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

#[cfg(test)]
mod tests {
    use super::{AssetKind, asset_kind, comparable_versions, installed_matches_tag};

    #[test]
    fn asset_kind_accepts_raw_binary_urls() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64"),
            AssetKind::Plain
        );
    }

    #[test]
    fn asset_kind_accepts_gzip_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.gz"),
            AssetKind::TarGz
        );
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tgz"),
            AssetKind::TarGz
        );
    }

    #[test]
    fn asset_kind_accepts_bzip2_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.bz2"),
            AssetKind::TarBz2
        );
    }

    #[test]
    fn asset_kind_accepts_zstd_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.zst"),
            AssetKind::TarZst
        );
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tzst"),
            AssetKind::TarZst
        );
    }

    #[test]
    fn asset_kind_accepts_xz_tar_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.tar.xz"),
            AssetKind::TarXz
        );
    }

    #[test]
    fn asset_kind_rejects_xz_compressed_singles_until_reference_supports_them() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.xz"),
            AssetKind::Unsupported
        );
    }

    #[test]
    fn asset_kind_accepts_gzip_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.gz"),
            AssetKind::Gz
        );
    }

    #[test]
    fn asset_kind_accepts_bzip2_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.bz2"),
            AssetKind::Bz2
        );
    }

    #[test]
    fn asset_kind_accepts_zstd_compressed_singles() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.zst"),
            AssetKind::Zst
        );
    }

    #[test]
    fn asset_kind_accepts_zip_archives() {
        assert_eq!(
            asset_kind("https://example.com/tool-linux-x86_64.zip"),
            AssetKind::Zip
        );
    }

    #[test]
    fn release_tag_comparison_accepts_common_github_prefixes() {
        assert!(installed_matches_tag("1.2.3", "v1.2.3"));
        assert!(installed_matches_tag("0.133.0", "rust-v0.133.0"));
        assert!(installed_matches_tag("2026.5.15", "release-2026.5.15"));
        assert!(installed_matches_tag(
            "nightly-20260525",
            "nightly-20260525"
        ));
        assert!(!installed_matches_tag("1.2.2", "v1.2.3"));

        assert_eq!(
            comparable_versions("rust-v0.133.0"),
            vec!["rust-v0.133.0", "0.133.0"]
        );
    }
}
