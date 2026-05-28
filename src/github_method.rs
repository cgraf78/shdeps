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

use crate::Result;
use crate::config::Entry;
use crate::github;
use crate::github_release;
use crate::http::Client;
use crate::jobs;
use crate::manifest::Manifest;
use crate::platform::{self, RuntimeEnv};
use crate::process::Runner;
use crate::runtime::{Env, Roots};
use crate::stamp;
use crate::state;

const METHOD_RELEASE: &str = "github:release";
const METHOD_REPO: &str = "github:repo";
const METHOD_REPO_NO_ASSET: &str = "github:repo:no-compatible-release";
const METHOD_REPO_LAST_KNOWN: &str = "github:repo:last-known";
const RESOLVE_KIND: &str = "github";

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
    /// Runtime platform/host identity.
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
        .filter(|entry| entry.method == "github" && active(entry, context.env))
        .count();
    if total == 0 {
        return resolve_entries(entries, context, options);
    }

    progress(0, total)?;

    let mut done = 0usize;
    let mut resolved = vec![None; entries.len()];
    let mut remote = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.method != "github" {
            resolved[index] = Some(entry.clone());
            continue;
        }
        if !active(entry, context.env) {
            resolved[index] = Some(resolved_entry(entry, METHOD_REPO));
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
    let token = github::token(&env, context.runner);
    let remote_results = jobs::parallel_map_with_progress(
        &remote,
        max_jobs,
        |candidate| RemoteProbe {
            index: candidate.index,
            releases: github::fetch_releases_with_token(
                &candidate.name,
                context.client,
                token.as_deref(),
            )
            .ok(),
        },
        |completed| progress(done + completed, total),
    )?;

    for probe in remote_results {
        let entry = &entries[probe.index];
        let cache = Cache::new(&context.roots.state_dir, &entry.name);
        let method = resolve_remote_method(entry, context, &cache, options, probe.releases)?;
        resolved[probe.index] = Some(resolved_entry(entry, method));
    }

    Ok(collect_resolved(resolved))
}

#[derive(Debug, Clone)]
struct RemoteCandidate {
    index: usize,
    name: String,
}

struct RemoteProbe {
    index: usize,
    releases: Option<Vec<github::Release>>,
}

/// Resolves one entry, leaving explicit concrete methods untouched.
pub fn resolve_entry<R>(entry: &Entry, context: &Context<'_, R>, options: Options) -> Result<Entry>
where
    R: Runner,
{
    if entry.method != "github" {
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
        return Ok(METHOD_REPO);
    }

    let cache = Cache::new(&context.roots.state_dir, &entry.name);
    if let Some(method) = resolve_local_method(entry, context, &cache, options)? {
        return Ok(method);
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
        if let Some(method) = cache.read_method()? {
            if let Some(method) = usable_cached_method(method, entry, context) {
                return Ok(Some(method));
            }
        }
    }
    if let Some(method) = seed_method_from_manifest(entry, context, cache, options)? {
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
                cache.write_method(METHOD_RELEASE)?;
                stamp::remote_touch(&cache.stamp, options.now)?;
                METHOD_RELEASE
            } else {
                // This is the only repo fallback worth caching: GitHub
                // answered successfully, and the current host has no matching
                // release asset. Cache it with an explicit reason so old
                // "github:repo" cache files written after fetch failures are
                // not trusted forever.
                github::write_cached_releases(&context.roots.state_dir, &entry.name, &releases)?;
                cache.write_method(METHOD_REPO_NO_ASSET)?;
                stamp::remote_touch(&cache.stamp, options.now)?;
                METHOD_REPO
            }
        }
        None => {
            // Prefer the last proven concrete method during transient GitHub
            // failures. A stale cache is not authoritative enough to skip a
            // successful remote re-check, but it is much better than flipping
            // an installed release-backed CLI to a source checkout just
            // because the fleet exhausted the unauthenticated API quota.
            if let Some(method) = cache.read_stale_method()? {
                usable_cached_method(method, entry, context).unwrap_or(METHOD_REPO)
            } else {
                // With no prior signal, keep the historical soft fallback so
                // first-time installs can still try a source checkout when the
                // API is unavailable.
                METHOD_REPO
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
            stamp: stamp::remote_path(state_dir, name, RESOLVE_KIND),
        }
    }

    fn read_method(&self) -> Result<Option<&'static str>> {
        self.read_method_file(false)
    }

    fn read_stale_method(&self) -> Result<Option<&'static str>> {
        self.read_method_file(true)
    }

    fn read_method_file(&self, allow_legacy_repo: bool) -> Result<Option<&'static str>> {
        let content = match fs::read_to_string(&self.method) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(match content.trim_end() {
            METHOD_RELEASE => Some(METHOD_RELEASE),
            METHOD_REPO_NO_ASSET => Some(METHOD_REPO),
            METHOD_REPO_LAST_KNOWN => Some(METHOD_REPO_LAST_KNOWN),
            // Legacy cache files wrote plain `github:repo` for both "no
            // compatible release" and "metadata fetch failed". Re-probe those
            // once so hosts can recover after credentials or repo visibility
            // change, then rewrite successful no-asset fallbacks with the
            // reasoned marker above.
            METHOD_REPO if allow_legacy_repo => Some(METHOD_REPO),
            METHOD_REPO => None,
            _ => None,
        })
    }

    fn write_method(&self, method: &str) -> Result<()> {
        state::write_atomic(&self.method, &format!("{method}\n"))
    }
}

fn seed_method_from_manifest<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    cache: &Cache,
    options: Options,
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

    let Some((stored, resolved)) = manifest_method(row, context.roots, context.runner) else {
        return Ok(None);
    };

    // Bare `github` was introduced after many machines already had concrete
    // manifest rows. Trusting that local install state for one TTL avoids a
    // fleet-wide burst of GitHub release probes just to rediscover "this repo
    // has always been a source checkout." The next stale/forced run still
    // performs the normal remote check, so this is a bootstrap cache, not a
    // permanent policy override.
    cache.write_method(stored)?;
    stamp::remote_touch(&cache.stamp, options.now)?;
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
            Some(METHOD_REPO)
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
    row.cmd == entry.cmd && manifest_method(row, context.roots, context.runner).is_some()
}

fn manifest_method<R>(
    row: &crate::manifest::ManifestEntry,
    roots: &Roots,
    runner: &R,
) -> Option<(&'static str, &'static str)>
where
    R: Runner,
{
    match row.method.as_str() {
        METHOD_REPO if manifest_repo_command_visible(row, roots, runner) => {
            Some((METHOD_REPO_LAST_KNOWN, METHOD_REPO))
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
    let repo_path = manifest_repo_path(row, roots);
    let Some(command_path) = runner.path(&row.cmd) else {
        return false;
    };

    // The manifest seed is a rollout optimization, not proof of correctness.
    // Source checkouts often contain many executable helper scripts, and an
    // unrelated package can also put the same command name on PATH. Trust a
    // cached repo answer only when shell lookup resolves back into the repo
    // path recorded in the manifest.
    canonical_child_of(&command_path, &repo_path)
}

fn manifest_repo_path(row: &crate::manifest::ManifestEntry, roots: &Roots) -> PathBuf {
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
        fixture.write_cache("owner/tool", "github:release", 10);
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
    fn cached_no_asset_repo_resolution_avoids_repeated_github_fetches() {
        let fixture = Fixture::new("warm-repo-cache");
        fixture.write_cache("owner/tool", "github:repo:no-compatible-release", 10);
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
        fixture.write_cache("owner/tool", "github:repo:last-known", 10);
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
    fn fresh_last_known_repo_cache_requires_manifest_command_path() {
        let fixture = Fixture::new("last-known-repo-missing-command");
        fixture.write_cache("owner/tool", "github:repo:last-known", 10);
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
        fixture.write_cache("owner/tool", "github:release", 10);
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
                &stamp::remote_path(&self.roots.state_dir, name, "github"),
                now,
            )
            .unwrap();
        }

        fn cached_method(&self, name: &str) -> String {
            fs::read_to_string(self.roots.state_dir.join(format!("{name}.github.method")))
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        }

        fn write_repo_command(&self, name: &str, command: &str) -> PathBuf {
            let command_path = self.roots.install_dir.join(name).join("bin").join(command);
            fs::create_dir_all(command_path.parent().unwrap()).unwrap();
            fs::write(&command_path, b"fake executable").unwrap();
            command_path
        }

        fn write_outside_command(&self, command: &str) -> PathBuf {
            let command_path = self.root.join("outside-bin").join(command);
            fs::create_dir_all(command_path.parent().unwrap()).unwrap();
            fs::write(&command_path, b"fake executable").unwrap();
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
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                uname: "x86_64".to_owned(),
                missing: BTreeSet::new(),
                paths: BTreeMap::new(),
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
            match (program, args) {
                ("uname", ["-m"]) => Ok(Output {
                    success: true,
                    timed_out: false,
                    stdout: format!("{}\n", self.uname),
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
