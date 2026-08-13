//! Resolver for the config-level `github` install method.
//!
//! Bare `github` is intentionally not an installed method. It is config sugar
//! for "use the best GitHub-backed concrete method on this host." Resolving it
//! before update/status/prune keeps the rest of the system simple: manifests,
//! method-transition cleanup, status output, and install code continue to see
//! only `github:release` or `github:repo`, which are the methods that actually
//! own files on disk.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::Result;
use crate::config::Entry;
use crate::github;
use crate::github_release;
use crate::http::Client;
use crate::jobs;
use crate::manifest::Manifest;
use crate::method;
use crate::platform::{self, RuntimeEnv};
use crate::process::{self, Runner};
use crate::runtime::{Env, Roots};
use crate::stamp;
use crate::state;

const METHOD_REPO_NO_ASSET: &str = "github:repo:no-compatible-release";
const METHOD_REPO_LAST_KNOWN: &str = "github:repo:last-known";

/// Runtime flags for `github` method resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Bypass cached resolver decisions.
    pub force: bool,
    /// Reinstall also bypasses cache because update/install work is explicit.
    pub reinstall: bool,
    /// Current epoch seconds used for TTL stamps.
    pub now: u64,
    /// Resolver cache TTL in seconds.
    pub remote_ttl: u64,
}

impl Options {
    fn freshness(self) -> stamp::Freshness {
        stamp::Freshness {
            now: self.now,
            ttl: self.remote_ttl,
            force: self.force,
            reinstall: self.reinstall,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            force: false,
            reinstall: false,
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            remote_ttl: 3600,
        }
    }
}

/// Inputs needed to resolve bare `github` entries.
pub struct Context<'a, R: Runner> {
    /// Runtime filesystem roots.
    pub roots: &'a Roots,
    /// Existing install manifest, used to seed bare `github` caches.
    pub manifest: Option<&'a Manifest>,
    /// Runtime platform, host, and package-manager identity.
    pub env: &'a RuntimeEnv,
    /// Environment variables used for GitHub credentials.
    pub env_vars: &'a BTreeMap<String, String>,
    /// Host subprocess runner used by GitHub token and asset matching logic.
    pub runner: &'a R,
    /// HTTP client used to fetch GitHub release metadata.
    pub client: &'a dyn Client,
}

/// Resolves all bare `github` entries to concrete install methods.
pub fn resolve_entries<R>(
    entries: &[Entry],
    context: &Context<'_, R>,
    options: Options,
) -> Result<Vec<Entry>>
where
    R: Runner,
{
    entries
        .iter()
        .map(|entry| resolve_entry(entry, context, options))
        .collect()
}

/// Resolves bare `github` entries while reporting active dependency progress.
///
/// Cache checks and cache writes stay on the caller thread. Only the remote
/// metadata fetches run in worker threads, which keeps state mutation ordered
/// while still overlapping the rate-limited network work that dominates forced
/// updates.
pub fn resolve_entries_with_progress<R, P>(
    entries: &[Entry],
    context: &Context<'_, R>,
    options: Options,
    max_jobs: usize,
    mut progress: P,
) -> Result<Vec<Entry>>
where
    R: Runner + Sync,
    P: FnMut(usize, usize) -> Result<()>,
{
    let total = entries
        .iter()
        .filter(|entry| entry.method == method::GITHUB && active(entry, context.env))
        .count();
    if total == 0 {
        return resolve_entries(entries, context, options);
    }

    progress(0, total)?;

    let mut done = 0usize;
    let mut resolved = vec![None; entries.len()];
    let mut remote = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.method != method::GITHUB {
            resolved[index] = Some(entry.clone());
            continue;
        }
        if !active(entry, context.env) {
            resolved[index] = Some(resolved_entry(entry, method::GITHUB_REPO));
            continue;
        }

        let cache = Cache::new(&context.roots.state_dir, &entry.name);
        if let Some(method) = resolve_local_method(entry, context, &cache, options)? {
            resolved[index] = Some(resolved_entry(entry, method));
            done += 1;
            progress(done, total)?;
        } else {
            remote.push(RemoteCandidate {
                index,
                name: entry.name.clone(),
                cmd: entry.cmd.clone(),
            });
        }
    }

    if remote.is_empty() {
        return Ok(collect_resolved(resolved));
    }

    let env = EnvVars {
        vars: context.env_vars,
        runtime: context.env,
    };
    let token = OnceLock::new();
    let remote_results = jobs::parallel_map_with_progress(
        &remote,
        max_jobs,
        |candidate| {
            let current_release = confirmed_current_release(candidate, context, options);
            RemoteProbe {
                index: candidate.index,
                current_release,
                releases: (!current_release)
                    .then(|| {
                        let token = token
                            .get_or_init(|| github::token(&env, context.runner))
                            .as_deref();
                        github::fetch_releases_with_token(&candidate.name, context.client, token)
                            .ok()
                    })
                    .flatten(),
            }
        },
        |completed| progress(done + completed, total),
    )?;

    for probe in remote_results {
        let entry = &entries[probe.index];
        let cache = Cache::new(&context.roots.state_dir, &entry.name);
        let method = if probe.current_release
            && github::remove_cached_releases(&context.roots.state_dir, &entry.name).is_ok()
        {
            // The installed executable already proves this host can run the
            // release method. Keep it while GitHub's public latest tag is
            // unchanged; a new tag falls through to authoritative asset
            // selection before this cache is refreshed again.
            cache.write_method(method::GITHUB_RELEASE, entry)?;
            stamp::remote_touch(&cache.stamp, options.now)?;
            stamp::remote_touch(
                &stamp::remote_path(&context.roots.state_dir, &entry.name, "release"),
                options.now,
            )?;
            method::GITHUB_RELEASE
        } else {
            resolve_remote_method(entry, context, &cache, options, probe.releases)?
        };
        resolved[probe.index] = Some(resolved_entry(entry, method));
    }

    Ok(collect_resolved(resolved))
}

#[derive(Debug, Clone)]
struct RemoteCandidate {
    index: usize,
    name: String,
    cmd: String,
}

struct RemoteProbe {
    index: usize,
    current_release: bool,
    releases: Option<Vec<github::Release>>,
}

/// Resolves one entry, leaving explicit concrete methods untouched.
pub fn resolve_entry<R>(entry: &Entry, context: &Context<'_, R>, options: Options) -> Result<Entry>
where
    R: Runner,
{
    if entry.method != method::GITHUB {
        return Ok(entry.clone());
    }

    let method = resolve_method(entry, context, options)?;
    let mut resolved = entry.clone();
    resolved.method = method.to_owned();
    Ok(resolved)
}

fn resolve_method<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    options: Options,
) -> Result<&'static str>
where
    R: Runner,
{
    if !active(entry, context.env) {
        // A filtered dependency does not own artifacts on this host, so there
        // is no useful remote fact to learn. Still return a concrete method so
        // `shdeps list` never exposes the config-only meta-method.
        return Ok(method::GITHUB_REPO);
    }

    let cache = Cache::new(&context.roots.state_dir, &entry.name);
    if let Some(method) = resolve_local_method(entry, context, &cache, options)? {
        return Ok(method);
    }

    let candidate = RemoteCandidate {
        index: 0,
        name: entry.name.clone(),
        cmd: entry.cmd.clone(),
    };
    if confirmed_current_release(&candidate, context, options)
        && github::remove_cached_releases(&context.roots.state_dir, &entry.name).is_ok()
    {
        cache.write_method(method::GITHUB_RELEASE, entry)?;
        stamp::remote_touch(&cache.stamp, options.now)?;
        stamp::remote_touch(
            &stamp::remote_path(&context.roots.state_dir, &entry.name, "release"),
            options.now,
        )?;
        return Ok(method::GITHUB_RELEASE);
    }

    let env = EnvVars {
        vars: context.env_vars,
        runtime: context.env,
    };
    resolve_remote_method(
        entry,
        context,
        &cache,
        options,
        github::fetch_releases(&entry.name, &env, context.runner, context.client).ok(),
    )
}

fn confirmed_current_release<R>(
    candidate: &RemoteCandidate,
    context: &Context<'_, R>,
    options: Options,
) -> bool
where
    R: Runner,
{
    if options.reinstall {
        return false;
    }
    let Some(manifest) = context.manifest else {
        return false;
    };
    let Some(row) = manifest.get(&candidate.name) else {
        return false;
    };
    if row.method != method::GITHUB_RELEASE || row.cmd != candidate.cmd {
        return false;
    }
    let public_bin = context.roots.bin_dir.join(&candidate.cmd);
    if !process::executable_path(&public_bin) {
        return false;
    }
    let Some(version) = process::dep_version(context.runner, &candidate.cmd) else {
        return false;
    };
    github::latest_release_matches(&candidate.name, &version, context.client).unwrap_or(false)
}

fn resolve_local_method<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    cache: &Cache,
    options: Options,
) -> Result<Option<&'static str>>
where
    R: Runner,
{
    if stamp::remote_fresh(&cache.stamp, options.freshness()) {
        let cached_method_file_exists = cache.method_file_exists()?;
        if let Some(method) = cache.read_method(entry)? {
            if let Some(method) = usable_cached_method(method, entry, context) {
                return Ok(Some(method));
            }
        }
        if cached_method_file_exists {
            return Ok(None);
        }
    }
    if let Some(method) = seed_method_from_manifest(entry, context, cache, options, false)? {
        return Ok(Some(method));
    }

    Ok(None)
}

fn resolve_remote_method<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    cache: &Cache,
    options: Options,
    releases: Option<Vec<github::Release>>,
) -> Result<&'static str>
where
    R: Runner,
{
    let method = match releases {
        Some(releases) => {
            if github_release::select(&entry.cmd, &releases, context.env, context.runner).is_some()
            {
                // Release assets are the preferred fleet path because they
                // avoid requiring git source trees or a local toolchain. A
                // local checkout under SHDEPS_GIT_DEV_DIR must not override
                // this decision; users who want live-checkout behavior should
                // ask for `github:repo` explicitly.
                github::write_cached_releases(&context.roots.state_dir, &entry.name, &releases)?;
                cache.write_method(method::GITHUB_RELEASE, entry)?;
                stamp::remote_touch(&cache.stamp, options.now)?;
                method::GITHUB_RELEASE
            } else {
                // This is the only repo fallback worth caching: GitHub
                // answered successfully, and the current host has no matching
                // release asset. Cache it with an explicit reason so old
                // "github:repo" cache files written after fetch failures are
                // not trusted forever.
                github::write_cached_releases(&context.roots.state_dir, &entry.name, &releases)?;
                cache.write_method(METHOD_REPO_NO_ASSET, entry)?;
                stamp::remote_touch(&cache.stamp, options.now)?;
                method::GITHUB_REPO
            }
        }
        None => {
            // Prefer the last proven concrete method during transient GitHub
            // failures. A stale cache is not authoritative enough to skip a
            // successful remote re-check, but it is much better than flipping
            // an installed release-backed CLI to a source checkout just
            // because the fleet exhausted the unauthenticated API quota.
            if let Some(method) = cache.read_stale_method(entry)? {
                usable_cached_method(method, entry, context).unwrap_or(method::GITHUB_REPO)
            } else if let Some(method) =
                seed_method_from_manifest(entry, context, cache, options, true)?
            {
                method
            } else {
                // With no prior signal, keep the historical soft fallback so
                // first-time installs can still try a source checkout when the
                // API is unavailable.
                method::GITHUB_REPO
            }
        }
    };
    Ok(method)
}

fn resolved_entry(entry: &Entry, method: &str) -> Entry {
    let mut resolved = entry.clone();
    resolved.method = method.to_owned();
    resolved
}

fn collect_resolved(entries: Vec<Option<Entry>>) -> Vec<Entry> {
    entries
        .into_iter()
        .map(|entry| entry.expect("every entry is resolved before collection"))
        .collect()
}

fn active(entry: &Entry, env: &RuntimeEnv) -> bool {
    matches!(
        platform::filter_match(&entry.filter, env),
        platform::FilterMatch::Match
    )
}

struct Cache {
    method: PathBuf,
    stamp: PathBuf,
}

impl Cache {
    fn new(state_dir: &Path, name: &str) -> Self {
        Self {
            // This is resolver cache state, not manifest metadata. The manifest
            // remains the installed-state ledger and stores only the concrete
            // method that owns files; this cache only avoids repeating GitHub
            // release probes on warm `github` config entries.
            method: state_dir.join(format!("{name}.github.method")),
            stamp: stamp::remote_path(state_dir, name, method::GITHUB),
        }
    }

    fn method_file_exists(&self) -> Result<bool> {
        match fs::metadata(&self.method) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn read_method(&self, entry: &Entry) -> Result<Option<&'static str>> {
        self.read_method_file(entry, false)
    }

    fn read_stale_method(&self, entry: &Entry) -> Result<Option<&'static str>> {
        self.read_method_file(entry, true)
    }

    fn read_method_file(
        &self,
        entry: &Entry,
        allow_legacy_repo: bool,
    ) -> Result<Option<&'static str>> {
        let content = match fs::read_to_string(&self.method) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let cached = CachedMethod::parse(&content);
        Ok(match cached.method {
            method::GITHUB_RELEASE if cached.matches_cmd(entry) => Some(method::GITHUB_RELEASE),
            method::GITHUB_RELEASE => None,
            METHOD_REPO_NO_ASSET if cached.matches_cmd(entry) => Some(method::GITHUB_REPO),
            METHOD_REPO_NO_ASSET => None,
            METHOD_REPO_LAST_KNOWN if cached.matches_cmd(entry) => Some(METHOD_REPO_LAST_KNOWN),
            METHOD_REPO_LAST_KNOWN => None,
            // Legacy cache files wrote plain `github:repo` for both "no
            // compatible release" and "metadata fetch failed". Re-probe those
            // once so hosts can recover after credentials or repo visibility
            // change, then rewrite successful no-asset fallbacks with the
            // reasoned marker above.
            method::GITHUB_REPO if allow_legacy_repo => Some(method::GITHUB_REPO),
            method::GITHUB_REPO => None,
            _ => None,
        })
    }

    fn write_method(&self, method: &str, entry: &Entry) -> Result<()> {
        state::write_atomic(&self.method, &format!("{method}\ncmd={}\n", entry.cmd))
    }
}

struct CachedMethod<'a> {
    method: &'a str,
    cmd: Option<&'a str>,
}

impl<'a> CachedMethod<'a> {
    fn parse(content: &'a str) -> Self {
        let mut lines = content.lines();
        let method = lines.next().unwrap_or_default().trim();
        let cmd = lines.find_map(|line| line.strip_prefix("cmd="));
        Self { method, cmd }
    }

    fn matches_cmd(&self, entry: &Entry) -> bool {
        self.cmd.is_some_and(|cmd| cmd == entry.cmd)
    }
}

fn seed_method_from_manifest<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    cache: &Cache,
    options: Options,
    allow_release_seed: bool,
) -> Result<Option<&'static str>>
where
    R: Runner,
{
    if options.force || options.reinstall {
        return Ok(None);
    }

    let Some(manifest) = context.manifest else {
        return Ok(None);
    };
    let Some(row) = manifest.get(&entry.name) else {
        return Ok(None);
    };
    if row.cmd != entry.cmd {
        return Ok(None);
    }

    let Some((stored, resolved)) =
        manifest_method(row, context.roots, context.runner, allow_release_seed)
    else {
        return Ok(None);
    };

    // Bare `github` was introduced after many machines already had concrete
    // manifest rows. Trusting that local install state for one TTL avoids a
    // fleet-wide burst of GitHub release probes just to rediscover "this repo
    // has always been a source checkout." The next stale/forced run still
    // performs the normal remote check, so this is a bootstrap cache, not a
    // permanent policy override.
    // A release manifest fallback after a failed metadata fetch is only a
    // local recovery signal. Do not cache it as though we proved current
    // release compatibility remotely, or the next healthy run may skip the
    // release-vs-repo decision and delay a required transition.
    if !(allow_release_seed && stored == method::GITHUB_RELEASE) {
        cache.write_method(stored, entry)?;
        stamp::remote_touch(&cache.stamp, options.now)?;
    }
    Ok(Some(resolved))
}

fn usable_cached_method<R>(
    method: &'static str,
    entry: &Entry,
    context: &Context<'_, R>,
) -> Option<&'static str>
where
    R: Runner,
{
    match method {
        METHOD_REPO_LAST_KNOWN if last_known_repo_still_matches(entry, context) => {
            Some(method::GITHUB_REPO)
        }
        METHOD_REPO_LAST_KNOWN => None,
        _ => Some(method),
    }
}

fn last_known_repo_still_matches<R>(entry: &Entry, context: &Context<'_, R>) -> bool
where
    R: Runner,
{
    let Some(manifest) = context.manifest else {
        return false;
    };
    let Some(row) = manifest.get(&entry.name) else {
        return false;
    };
    row.cmd == entry.cmd && manifest_method(row, context.roots, context.runner, false).is_some()
}

fn manifest_method<R>(
    row: &crate::manifest::ManifestEntry,
    roots: &Roots,
    runner: &R,
    allow_release: bool,
) -> Option<(&'static str, &'static str)>
where
    R: Runner,
{
    match row.method.as_str() {
        method::GITHUB_RELEASE if allow_release && manifest_release_command_visible(row) => {
            Some((method::GITHUB_RELEASE, method::GITHUB_RELEASE))
        }
        method::GITHUB_REPO if manifest_repo_command_visible(row, roots, runner) => {
            Some((METHOD_REPO_LAST_KNOWN, method::GITHUB_REPO))
        }
        _ => None,
    }
}

fn manifest_repo_command_visible<R>(
    row: &crate::manifest::ManifestEntry,
    roots: &Roots,
    runner: &R,
) -> bool
where
    R: Runner,
{
    manifest_command_visible(row, roots, runner)
}

fn manifest_release_command_visible(row: &crate::manifest::ManifestEntry) -> bool {
    !row.install_path.is_empty() && crate::process::executable_path(Path::new(&row.install_path))
}

fn manifest_command_visible<R>(
    row: &crate::manifest::ManifestEntry,
    roots: &Roots,
    runner: &R,
) -> bool
where
    R: Runner,
{
    let install_path = manifest_install_path(row, roots);
    let Some(command_path) = runner.path(&row.cmd) else {
        return false;
    };

    // The manifest seed is a rollout optimization, not proof of correctness.
    // Source checkouts often contain many executable helper scripts, and an
    // unrelated package can also put the same command name on PATH. Trust a
    // local manifest answer only when shell lookup resolves back into the
    // install path recorded in the manifest.
    canonical_child_of(&command_path, &install_path)
}

fn manifest_install_path(row: &crate::manifest::ManifestEntry, roots: &Roots) -> PathBuf {
    if row.install_path.is_empty() {
        roots.install_dir.join(&row.name)
    } else {
        PathBuf::from(&row.install_path)
    }
}

fn canonical_child_of(child: &Path, parent: &Path) -> bool {
    let Ok(child) = fs::canonicalize(child) else {
        return false;
    };
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };
    child.starts_with(parent)
}

struct EnvVars<'a> {
    vars: &'a BTreeMap<String, String>,
    runtime: &'a RuntimeEnv,
}

impl Env for EnvVars<'_> {
    fn var_os(&self, name: &str) -> Option<OsString> {
        self.vars.get(name).map(OsString::from)
    }

    fn command_output(&self, command: &str, args: &[&str]) -> Option<String> {
        match (command, args) {
            ("uname", ["-s"]) => Some(match self.runtime.platform() {
                "macos" => "Darwin".to_owned(),
                "wsl" | "linux" => "Linux".to_owned(),
                other => other.to_owned(),
            }),
            ("hostname", []) => Some(self.runtime.host().to_owned()),
            _ => None,
        }
    }

    fn read_to_string(&self, _path: &Path) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{Context, Options, resolve_entries, resolve_entries_with_progress};
    use crate::config::parse_entry;
    use crate::github;
    use crate::http::Client;
    use crate::manifest::Manifest;
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;
    use crate::stamp;

    #[test]
    fn github_resolves_to_release_when_compatible_asset_exists() {
        let fixture = Fixture::new("release");
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(
            github::read_cached_releases(&fixture.roots.state_dir, "owner/tool").unwrap()[0].tag,
            "v1.0.0"
        );
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn github_resolves_to_repo_when_no_compatible_asset_exists() {
        let fixture = Fixture::new("repo-fallback");
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-darwin-aarch64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert_eq!(
            fixture.cached_method("owner/tool"),
            "github:repo:no-compatible-release"
        );
    }

    #[test]
    fn github_resolves_to_repo_when_release_metadata_fetch_fails() {
        let fixture = Fixture::new("fetch-fallback");
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert!(fixture.cached_method("owner/tool").is_empty());
    }

    #[test]
    fn explicit_github_methods_are_not_resolved_or_fetched() {
        let fixture = Fixture::new("explicit");
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![
            parse_entry("owner/tool|github:release|tool|-|-", None),
            parse_entry("owner/repo|github:repo|repo|-|-", None),
        ];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved, entries);
        assert!(client.urls().is_empty());
    }

    #[test]
    fn cached_resolution_avoids_repeated_github_fetches() {
        let fixture = Fixture::new("warm-cache");
        fixture.write_cache_for_cmd("owner/tool", "github:release", "tool", 10);
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert!(client.urls().is_empty());
    }

    #[test]
    fn cached_release_resolution_rechecks_when_command_changes() {
        let fixture = Fixture::new("warm-release-cache-command-change");
        fixture.write_cache_for_cmd("owner/tool", "github:release", "tool", 10);
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["othercmd-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|othercmd|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(
            fixture.cached_cmd("owner/tool"),
            Some("othercmd".to_owned())
        );
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn legacy_release_cache_is_rechecked_so_command_compatibility_is_verified() {
        let fixture = Fixture::new("legacy-release-cache");
        fixture.write_cache("owner/tool", "github:release", 10);
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["othercmd-v1.0.0-darwin-aarch64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert_eq!(
            fixture.cached_method("owner/tool"),
            "github:repo:no-compatible-release"
        );
        assert_eq!(fixture.cached_cmd("owner/tool"), Some("tool".to_owned()));
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn cached_no_asset_repo_resolution_avoids_repeated_github_fetches() {
        let fixture = Fixture::new("warm-repo-cache");
        fixture.write_cache_for_cmd(
            "owner/tool",
            "github:repo:no-compatible-release",
            "tool",
            10,
        );
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert!(client.urls().is_empty());
    }

    #[test]
    fn cached_no_asset_repo_resolution_rechecks_when_command_changes() {
        let fixture = Fixture::new("warm-repo-cache-command-change");
        fixture.write_cache_for_cmd(
            "owner/tool",
            "github:repo:no-compatible-release",
            "tool",
            10,
        );
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["realcmd-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|realcmd|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn manifest_repo_seed_avoids_first_bare_github_probe() {
        let fixture = Fixture::new("manifest-repo-seed");
        let command_path = fixture.write_repo_command("owner/tool", "tool");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new();
        let runner = FakeRunner::new()
            .with_uname("x86_64")
            .with_path("tool", command_path);
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert_eq!(
            fixture.cached_method("owner/tool"),
            "github:repo:last-known"
        );
        assert!(client.urls().is_empty());
    }

    #[test]
    fn manifest_release_seed_survives_metadata_fetch_failure() {
        let fixture = Fixture::new("manifest-release-seed-failure");
        let command_path = fixture.write_release_command("owner/tool", "tool");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:release|tool|{}\n",
            command_path.display()
        ));
        let client = FakeClient::new();
        let runner = FakeRunner::new()
            .with_uname("x86_64")
            .with_path("tool", command_path);
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "");
        assert_eq!(fixture.cached_cmd("owner/tool"), None);
        assert!(
            !stamp::remote_path(
                &fixture.roots.state_dir,
                "owner/tool",
                crate::method::GITHUB
            )
            .exists()
        );
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn legacy_release_cache_survives_metadata_failure_without_path_lookup() {
        let fixture = Fixture::new("legacy-release-cache-failure-no-path");
        fixture.write_cache("owner/tool", "github:release", 10);
        let command_path = fixture.write_release_command("owner/tool", "tool");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:release|tool|{}\n",
            command_path.display()
        ));
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(fixture.cached_cmd("owner/tool"), None);
        assert_eq!(
            fs::read_to_string(stamp::remote_path(
                &fixture.roots.state_dir,
                "owner/tool",
                crate::method::GITHUB
            ))
            .unwrap(),
            "10\n"
        );
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn manifest_repo_seed_requires_visible_command() {
        let fixture = Fixture::new("manifest-repo-seed-missing-command");
        fs::create_dir_all(fixture.roots.install_dir.join("owner/tool")).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64").missing("tool");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn manifest_repo_seed_rejects_unrelated_visible_command() {
        let fixture = Fixture::new("manifest-repo-seed-unrelated-command");
        fs::create_dir_all(fixture.roots.install_dir.join("owner/tool")).unwrap();
        let outside_command = fixture.write_outside_command("tool");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new()
            .with_uname("x86_64")
            .with_path("tool", outside_command);
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            fixture.options(false),
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn fresh_last_known_repo_cache_uses_manifest_command_path() {
        let fixture = Fixture::new("last-known-repo-cache-valid");
        fixture.write_cache_for_cmd("owner/tool", "github:repo:last-known", "tool", 10);
        let command_path = fixture.write_repo_command("owner/tool", "tool");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new();
        let runner = FakeRunner::new()
            .with_uname("x86_64")
            .with_path("tool", command_path);
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:repo");
        assert!(client.urls().is_empty());
    }

    #[test]
    fn fresh_last_known_repo_cache_rechecks_when_command_changes() {
        let fixture = Fixture::new("last-known-repo-cache-command-change");
        fixture.write_cache_for_cmd("owner/tool", "github:repo:last-known", "tool", 10);
        let command_path = fixture.write_repo_command("owner/tool", "othercmd");
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|othercmd|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["othercmd-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new()
            .with_uname("x86_64")
            .with_path("othercmd", command_path);
        let entries = vec![parse_entry("owner/tool|github|othercmd|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(
            fixture.cached_cmd("owner/tool"),
            Some("othercmd".to_owned())
        );
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn fresh_last_known_repo_cache_requires_manifest_command_path() {
        let fixture = Fixture::new("last-known-repo-missing-command");
        fixture.write_cache_for_cmd("owner/tool", "github:repo:last-known", "tool", 10);
        fs::create_dir_all(fixture.roots.install_dir.join("owner/tool")).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64").missing("tool");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn force_bypasses_manifest_seed() {
        let fixture = Fixture::new("manifest-seed-force");
        fs::create_dir_all(fixture.roots.install_dir.join("owner/tool")).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                force: true,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn stale_release_cache_survives_metadata_fetch_failures() {
        let fixture = Fixture::new("stale-release-cache");
        fixture.write_cache_for_cmd("owner/tool", "github:release", "tool", 10);
        let client = FakeClient::new();
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 4_000,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(
            client.urls(),
            vec![github::releases_url("owner/tool")],
            "stale caches should still try the remote before falling back"
        );
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
    }

    #[test]
    fn legacy_repo_cache_is_rechecked_so_release_assets_can_take_over() {
        let fixture = Fixture::new("legacy-repo-cache");
        fixture.write_cache("owner/tool", "github:repo", 10);
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn force_bypasses_cached_resolution() {
        let fixture = Fixture::new("force-cache");
        fixture.write_cache("owner/tool", "github:repo", 10);
        let client = FakeClient::new().with_releases(
            "owner/tool",
            releases_json("v1.0.0", &["tool-v1.0.0-linux-x86_64.tar.gz"]),
        );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context(&runner, &client),
            Options {
                force: true,
                now: 20,
                remote_ttl: 3600,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(client.urls(), vec![github::releases_url("owner/tool")]);
    }

    #[test]
    fn force_keeps_current_manifest_release_without_rest_or_token_probe() {
        use std::os::unix::fs::PermissionsExt;

        let mut fixture = Fixture::new("force-current-release");
        fixture
            .env_vars
            .insert("SHDEPS_ALLOW_GH_AUTH_TOKEN".to_owned(), "1".to_owned());
        let public_bin = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(public_bin.parent().unwrap()).unwrap();
        fs::write(&public_bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&public_bin, fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let stale_releases = github::parse_releases(&releases_json(
            "v9.0.0",
            &["tool-v9.0.0-linux-x86_64.tar.gz"],
        ))
        .unwrap();
        github::write_cached_releases(&fixture.roots.state_dir, "owner/tool", &stale_releases)
            .unwrap();
        let client = FakeClient::new().with_redirect("owner/tool", "v1.2.3");
        let runner = FakeRunner::new().with_version("tool", "1.2.3");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                force: true,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(fixture.cached_method("owner/tool"), "github:release");
        assert!(stamp::remote_checked_at(
            &stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            fixture.options(false).now,
        ));
        assert!(
            github::read_cached_releases(&fixture.roots.state_dir, "owner/tool").is_none(),
            "a public current-tag proof must invalidate stale REST asset metadata"
        );
        assert_eq!(
            client.urls(),
            vec![github::latest_release_url("owner/tool")]
        );
        assert!(!runner.calls().contains(&"gh auth token".to_owned()));
    }

    #[test]
    fn force_reselects_bare_github_method_when_latest_tag_changes() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("force-changed-release");
        let public_bin = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(public_bin.parent().unwrap()).unwrap();
        fs::write(&public_bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&public_bin, fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:release|tool|{}\n",
            fixture.roots.install_dir.join("owner/tool").display()
        ));
        let client = FakeClient::new()
            .with_redirect("owner/tool", "v1.2.3")
            .with_releases(
                "owner/tool",
                releases_json("v1.2.3", &["tool-v1.2.3-linux-x86_64.tar.gz"]),
            );
        let runner = FakeRunner::new().with_version("tool", "1.2.2");
        let entries = vec![parse_entry("owner/tool|github|tool|-|-", None)];

        let resolved = resolve_entries(
            &entries,
            &fixture.context_with_manifest(&runner, &client, &manifest),
            Options {
                force: true,
                ..fixture.options(false)
            },
        )
        .unwrap();

        assert_eq!(resolved[0].method, "github:release");
        assert_eq!(
            client.urls(),
            vec![
                github::latest_release_url("owner/tool"),
                github::releases_url("owner/tool"),
            ]
        );
    }

    #[test]
    fn progress_resolution_parallelizes_remote_fetches_but_writes_cache_in_order() {
        let fixture = Fixture::new("progress-parallel");
        let client = FakeClient::new()
            .with_delay(Duration::from_millis(25))
            .with_releases(
                "owner/tool-a",
                releases_json("v1.0.0", &["tool-a-v1.0.0-linux-x86_64.tar.gz"]),
            )
            .with_releases(
                "owner/tool-b",
                releases_json("v1.0.0", &["tool-b-v1.0.0-linux-x86_64.tar.gz"]),
            )
            .with_releases(
                "owner/tool-c",
                releases_json("v1.0.0", &["tool-c-v1.0.0-linux-x86_64.tar.gz"]),
            );
        let runner = FakeRunner::new().with_uname("x86_64");
        let entries = vec![
            parse_entry("owner/tool-a|github|tool-a|-|-", None),
            parse_entry("owner/tool-b|github|tool-b|-|-", None),
            parse_entry("owner/tool-c|github|tool-c|-|-", None),
        ];
        let mut progress = Vec::new();

        let resolved = resolve_entries_with_progress(
            &entries,
            &fixture.context(&runner, &client),
            fixture.options(true),
            2,
            |done, total| {
                progress.push((done, total));
                Ok(())
            },
        )
        .unwrap();

        assert!(
            resolved
                .iter()
                .all(|entry| entry.method == "github:release")
        );
        assert_eq!(progress.first(), Some(&(0, 3)));
        assert_eq!(progress.last(), Some(&(3, 3)));
        assert_eq!(fixture.cached_method("owner/tool-a"), "github:release");
        assert_eq!(fixture.cached_method("owner/tool-b"), "github:release");
        assert_eq!(fixture.cached_method("owner/tool-c"), "github:release");
        assert_eq!(
            client.max_active(),
            2,
            "bare-github method resolution should obey the requested job bound"
        );
    }

    struct Fixture {
        root: PathBuf,
        roots: Roots,
        env: RuntimeEnv,
        env_vars: BTreeMap<String, String>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "shdeps-github-method-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let roots = Roots {
                conf_dir: root.join("conf"),
                hooks_dir: root.join("conf/hooks.d"),
                state_dir: root.join("state"),
                git_dev_dir: root.join("git"),
                install_dir: root.join("share"),
                bin_dir: root.join("bin"),
                home: root.join("home"),
            };
            fs::create_dir_all(&roots.state_dir).unwrap();
            Self {
                root,
                roots,
                env: RuntimeEnv::new("linux", "host"),
                env_vars: BTreeMap::new(),
            }
        }

        fn context<'a>(
            &'a self,
            runner: &'a FakeRunner,
            client: &'a FakeClient,
        ) -> Context<'a, FakeRunner> {
            Context {
                roots: &self.roots,
                manifest: None,
                env: &self.env,
                env_vars: &self.env_vars,
                runner,
                client,
            }
        }

        fn context_with_manifest<'a>(
            &'a self,
            runner: &'a FakeRunner,
            client: &'a FakeClient,
            manifest: &'a Manifest,
        ) -> Context<'a, FakeRunner> {
            Context {
                roots: &self.roots,
                manifest: Some(manifest),
                env: &self.env,
                env_vars: &self.env_vars,
                runner,
                client,
            }
        }

        fn options(&self, force: bool) -> Options {
            Options {
                force,
                now: 1_700_000_000,
                remote_ttl: 3600,
                reinstall: false,
            }
        }

        fn write_cache(&self, name: &str, method: &str, now: u64) {
            let method_path = self.roots.state_dir.join(format!("{name}.github.method"));
            fs::create_dir_all(method_path.parent().unwrap()).unwrap();
            fs::write(method_path, format!("{method}\n")).unwrap();
            stamp::remote_touch(
                &stamp::remote_path(&self.roots.state_dir, name, crate::method::GITHUB),
                now,
            )
            .unwrap();
        }

        fn write_cache_for_cmd(&self, name: &str, method: &str, cmd: &str, now: u64) {
            let method_path = self.roots.state_dir.join(format!("{name}.github.method"));
            fs::create_dir_all(method_path.parent().unwrap()).unwrap();
            fs::write(method_path, format!("{method}\ncmd={cmd}\n")).unwrap();
            stamp::remote_touch(
                &stamp::remote_path(&self.roots.state_dir, name, crate::method::GITHUB),
                now,
            )
            .unwrap();
        }

        fn cached_method(&self, name: &str) -> String {
            fs::read_to_string(self.roots.state_dir.join(format!("{name}.github.method")))
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        }

        fn cached_cmd(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.roots.state_dir.join(format!("{name}.github.method")))
                .unwrap_or_default()
                .lines()
                .find_map(|line| line.strip_prefix("cmd=").map(ToOwned::to_owned))
        }

        fn write_repo_command(&self, name: &str, command: &str) -> PathBuf {
            let command_path = self.roots.install_dir.join(name).join("bin").join(command);
            fs::create_dir_all(command_path.parent().unwrap()).unwrap();
            fs::write(&command_path, b"fake executable").unwrap();
            let mut perms = fs::metadata(&command_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&command_path, perms).unwrap();
            command_path
        }

        fn write_release_command(&self, name: &str, command: &str) -> PathBuf {
            let command_path = self.roots.install_dir.join(name).join("bin").join(command);
            fs::create_dir_all(command_path.parent().unwrap()).unwrap();
            fs::write(&command_path, b"fake executable").unwrap();
            let mut perms = fs::metadata(&command_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&command_path, perms).unwrap();
            command_path
        }

        fn write_outside_command(&self, command: &str) -> PathBuf {
            let command_path = self.root.join("outside-bin").join(command);
            fs::create_dir_all(command_path.parent().unwrap()).unwrap();
            fs::write(&command_path, b"fake executable").unwrap();
            let mut perms = fs::metadata(&command_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&command_path, perms).unwrap();
            command_path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[derive(Default)]
    struct FakeClient {
        responses: BTreeMap<String, Vec<u8>>,
        redirects: BTreeMap<String, String>,
        urls: Arc<Mutex<Vec<String>>>,
        delay: Option<Duration>,
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
    }

    impl FakeClient {
        fn new() -> Self {
            Self::default()
        }

        fn with_releases(mut self, repo: &str, json: String) -> Self {
            self.responses
                .insert(github::releases_url(repo), json.into_bytes());
            self
        }

        fn with_redirect(mut self, repo: &str, tag: &str) -> Self {
            self.redirects.insert(
                github::latest_release_url(repo),
                format!("https://github.com/{repo}/releases/tag/{tag}"),
            );
            self
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = Some(delay);
            self
        }

        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }

        fn max_active(&self) -> usize {
            self.max_active.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Client for FakeClient {
        fn get(&self, url: &str, _token: Option<&str>) -> io::Result<Vec<u8>> {
            let active = self
                .active
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.max_active
                .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
            let _guard = ActiveGuard {
                active: &self.active,
            };
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            self.urls.lock().unwrap().push(url.to_owned());
            self.responses.get(url).cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("missing fake URL {url}"))
            })
        }

        fn redirect_location(&self, url: &str) -> io::Result<Option<String>> {
            self.urls.lock().unwrap().push(url.to_owned());
            Ok(self.redirects.get(url).cloned())
        }
    }

    struct ActiveGuard<'a> {
        active: &'a std::sync::atomic::AtomicUsize,
    }

    impl Drop for ActiveGuard<'_> {
        fn drop(&mut self) {
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        uname: String,
        missing: BTreeSet<String>,
        paths: BTreeMap<String, PathBuf>,
        versions: BTreeMap<String, String>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                uname: "x86_64".to_owned(),
                missing: BTreeSet::new(),
                paths: BTreeMap::new(),
                versions: BTreeMap::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_uname(mut self, uname: &str) -> Self {
            self.uname = uname.to_owned();
            self
        }

        fn missing(mut self, command: &str) -> Self {
            self.missing.insert(command.to_owned());
            self
        }

        fn with_path(mut self, command: &str, path: PathBuf) -> Self {
            self.paths.insert(command.to_owned(), path);
            self
        }

        fn with_version(mut self, command: &str, version: &str) -> Self {
            self.versions.insert(command.to_owned(), version.to_owned());
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, command: &str) -> bool {
            command != "ldd" && !self.missing.contains(command)
        }

        fn path(&self, command: &str) -> Option<PathBuf> {
            if !self.exists(command) {
                return None;
            }
            self.paths
                .get(command)
                .cloned()
                .or_else(|| Some(PathBuf::from(command)))
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            self.calls.lock().unwrap().push(
                std::iter::once(program)
                    .chain(args.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            match (program, args) {
                ("uname", ["-m"]) => Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: format!("{}\n", self.uname),
                    stderr: String::new(),
                }),
                (program, ["--version"]) if self.versions.contains_key(program) => Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: format!("{program} {}\n", self.versions[program]),
                    stderr: String::new(),
                }),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "missing fake command",
                )),
            }
        }
    }

    fn releases_json(tag: &str, assets: &[&str]) -> String {
        let assets = assets
            .iter()
            .map(|asset| {
                format!(r#"{{"name":"{asset}","browser_download_url":"https://github.com/owner/tool/releases/download/v1/{asset}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"[{{"tag_name":"{tag}","draft":false,"prerelease":false,"assets":[{assets}]}}]"#)
    }
}
