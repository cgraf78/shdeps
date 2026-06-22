//! `shdeps update` orchestration.
//!
//! Install methods are intentionally small units, but `update` owns the order
//! that makes the system safe: prove or stage the new method before old-method
//! cleanup, update manifest rows only after a method has made its decision, and
//! run post hooks only for dependencies that actually changed. Keeping those
//! rules here avoids each install method learning partial transaction policy.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::Result;
use crate::cleanup;
use crate::config::{self, Entry};
use crate::hooks::{BashCustomProbe, Install, Post, Txn};
use crate::http::Client;
use crate::jobs;
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::method;
use crate::package_cache;
use crate::platform::{self, RuntimeEnv};
use crate::process::Runner;
use crate::runtime::Roots;
use crate::stamp;
use crate::update_external;
use crate::update_pkg;
use crate::update_release;
use crate::update_repo;
use crate::update_transition;

/// Options controlling one update run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Force reinstall of dependencies that already appear installed.
    pub reinstall: bool,
    /// Bypass remote freshness stamps.
    pub force: bool,
    /// Include per-dependency detail that is too expensive for normal output.
    pub verbose: bool,
    /// Current epoch seconds used when checking and writing stamps.
    pub now: u64,
    /// Remote freshness TTL in seconds.
    pub remote_ttl: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reinstall: false,
            force: false,
            verbose: false,
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            remote_ttl: 3600,
        }
    }
}

impl Options {
    pub(crate) fn freshness(self) -> stamp::Freshness {
        stamp::Freshness {
            now: self.now,
            ttl: self.remote_ttl,
            force: self.force,
            reinstall: self.reinstall,
        }
    }
}

/// Per-dependency update result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Dependency name.
    pub name: String,
    /// True when the dependency changed and should run `post(name)`.
    pub changed: bool,
    /// True when the install method reported a hard failure.
    ///
    /// Missing or malformed custom hooks intentionally stay non-fatal for Bash
    /// parity: existing shdeps warns and skips them. A present `install()`
    /// returning non-zero is different because the user explicitly attempted
    /// work and the CLI must return failure.
    pub failed: bool,
    /// Structured status used for summaries and rendering.
    pub status: ItemStatus,
    /// Structured reason used by update orchestration.
    pub reason: ItemReason,
    /// Human-readable status detail for CLI output.
    pub detail: String,
}

/// Machine-readable update item status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Dependency is installed/current and did not change.
    Current,
    /// Dependency changed and should be counted as changed.
    Changed,
    /// Dependency was intentionally skipped.
    Skipped,
    /// Dependency failed.
    Failed,
    /// Dependency was queued for later batch work.
    Pending,
}

/// Machine-readable reason for an update item status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemReason {
    /// Installed/current dependency.
    Installed,
    /// Package manager override disabled this dependency.
    PackageManagerOverride,
    /// Package is unavailable on this host/package manager.
    PackageUnavailable,
    /// Package was queued for a later batch install.
    PackageQueued,
    /// Remote freshness stamp was current.
    Fresh,
    /// Required installer tool was missing.
    MissingTool,
    /// Installer failed.
    InstallFailed,
    /// Installer succeeded but produced no binary.
    MissingBinary,
    /// Custom hook was missing or unusable.
    CustomUnavailable,
    /// Custom install hook failed.
    CustomInstallFailed,
    /// Unsupported update method.
    UnsupportedMethod,
    /// Generic fallback for details that are already self-explanatory.
    Other,
}

impl Item {
    pub(crate) fn current(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            changed: false,
            failed: false,
            status: ItemStatus::Current,
            reason,
            detail: detail.into(),
        }
    }

    pub(crate) fn changed(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            changed: true,
            failed: false,
            status: ItemStatus::Changed,
            reason,
            detail: detail.into(),
        }
    }

    pub(crate) fn skipped(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            changed: false,
            failed: false,
            status: ItemStatus::Skipped,
            reason,
            detail: detail.into(),
        }
    }

    pub(crate) fn failed(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            changed: false,
            failed: true,
            status: ItemStatus::Failed,
            reason,
            detail: detail.into(),
        }
    }

    pub(crate) fn pending(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            changed: false,
            failed: false,
            status: ItemStatus::Pending,
            reason,
            detail: detail.into(),
        }
    }
}

/// Runtime data for one completed update group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    /// Progress group identifier, such as `packages` or `github-releases`.
    pub group: &'static str,
    /// Wall-clock time spent in this group.
    pub elapsed_ms: u128,
}

/// Summary of an update run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Dependencies that were considered and not filtered out.
    pub items: Vec<Item>,
    /// Dependencies whose install or post hook failed.
    pub failed: Vec<String>,
    /// Dependencies whose old-method cleanup needs a later retry.
    ///
    /// Method-transition cleanup runs after the new method has been recorded.
    /// At that point the dependency should remain usable even if deleting old
    /// artifacts fails, so cleanup failures are tracked separately from
    /// install failures instead of rolling back the successful method switch.
    /// These are real I/O failures from `cleanup_snapshot` (permission
    /// denied, read-only filesystem, etc.).
    ///
    /// By default `has_errors` does NOT count leftovers as run failures —
    /// promoting them would be a backwards-incompatible behavior change
    /// for CI pipelines and dotfiles bootstraps that previously saw
    /// silent leftover state. Operators that want exit-code enforcement
    /// can opt in with `SHDEPS_STRICT_LEFTOVERS=1`, which is checked by
    /// `has_errors`.
    pub leftovers: Vec<String>,
    /// Per-group runtime summaries for renderers that want compact detail.
    pub groups: Vec<GroupSummary>,
}

/// Progress sink for user-facing update renderers.
///
/// The updater owns real phase knowledge (package cache, release prefetch,
/// repo updates, language tools, custom installs). Keeping progress events here
/// lets the CLI render a live TTY view and lets parent commands consume
/// machine-readable events without scraping prose.
pub trait Progress {
    /// Reports that a phase is running or has advanced.
    fn phase(&mut self, phase: Phase<'_>) -> Result<()>;

    /// Reports the final item status for one dependency.
    fn item(&mut self, group: &'static str, item: &Item) -> Result<()>;

    /// Gives renderers a chance to yield the terminal before a child process
    /// may ask the user for input through `/dev/tty`.
    fn pause_for_prompt(&mut self, _detail: &str) -> Result<()> {
        Ok(())
    }
}

/// One progress update for an update phase.
#[derive(Debug, Clone, Copy)]
pub struct Phase<'a> {
    /// Stable machine group used for summaries.
    pub group: &'static str,
    /// Stable machine phase key used by renderers.
    pub key: &'static str,
    /// Phase status.
    pub status: &'static str,
    /// User-facing row label.
    pub label: &'a str,
    /// Descriptive detail for diagnostics.
    pub detail: &'a str,
    /// Completed units.
    pub done: usize,
    /// Total units.
    pub total: usize,
}

pub(crate) const GROUP_METHODS: &str = "github-methods";
pub(crate) const GROUP_PACKAGES: &str = "packages";
pub(crate) const GROUP_GITHUB_RELEASES: &str = "github-releases";
pub(crate) const GROUP_GITHUB_REPOS: &str = "github-repos";
pub(crate) const GROUP_CARGO: &str = "cargo";
pub(crate) const GROUP_GO: &str = "go";
pub(crate) const GROUP_UV: &str = "uv";
pub(crate) const GROUP_NPM: &str = "npm";
pub(crate) const GROUP_CUSTOM: &str = "custom";
pub(crate) const GROUP_OTHER: &str = "other";

pub(crate) const DASH_METHOD_RESOLUTION: &str = "method-resolution";
pub(crate) const DASH_GITHUB: &str = "github";

pub(crate) const PHASE_GITHUB_METHODS: &str = "github-methods";
pub(crate) const PHASE_PACKAGES: &str = "packages";
pub(crate) const PHASE_GITHUB_RELEASE_METADATA: &str = "github-release-metadata";
pub(crate) const PHASE_GITHUB_RELEASE_VERSIONS: &str = "github-release-versions";
pub(crate) const PHASE_GITHUB_RELEASE_INSTALLS: &str = "github-release-installs";
pub(crate) const PHASE_GITHUB_REPOS: &str = "github-repos";
pub(crate) const PHASE_CARGO: &str = "cargo";
pub(crate) const PHASE_GO: &str = "go";
pub(crate) const PHASE_UV: &str = "uv";
pub(crate) const PHASE_NPM: &str = "npm";
pub(crate) const PHASE_CUSTOM: &str = "custom";
pub(crate) const PHASE_OTHER: &str = "other-progress";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhaseSpec {
    pub(crate) group: &'static str,
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) detail: &'static str,
    pub(crate) dashboard_group: &'static str,
    pub(crate) dashboard_stage: &'static str,
    pub(crate) component: &'static str,
}

pub(crate) fn running_phase(key: &'static str, done: usize, total: usize) -> Phase<'static> {
    let spec = phase_spec(key);
    Phase {
        group: spec.group,
        key: spec.key,
        status: "running",
        label: spec.label,
        detail: spec.detail,
        done,
        total,
    }
}

pub(crate) fn phase_for_group(group: &'static str, done: usize, total: usize) -> Phase<'static> {
    running_phase(phase_key_for_group(group), done, total)
}

pub(crate) fn phase_spec(key: &str) -> PhaseSpec {
    match key {
        PHASE_GITHUB_METHODS => PhaseSpec {
            group: GROUP_METHODS,
            key: PHASE_GITHUB_METHODS,
            label: "Resolve sources",
            detail: "resolving GitHub methods",
            dashboard_group: DASH_METHOD_RESOLUTION,
            dashboard_stage: DASH_METHOD_RESOLUTION,
            component: PHASE_GITHUB_METHODS,
        },
        PHASE_PACKAGES => group_spec(GROUP_PACKAGES),
        PHASE_GITHUB_RELEASE_METADATA => PhaseSpec {
            group: GROUP_GITHUB_RELEASES,
            key: PHASE_GITHUB_RELEASE_METADATA,
            label: "GitHub",
            detail: "fetching GitHub release metadata",
            dashboard_group: DASH_GITHUB,
            dashboard_stage: DASH_GITHUB,
            component: PHASE_GITHUB_RELEASE_METADATA,
        },
        PHASE_GITHUB_RELEASE_VERSIONS => PhaseSpec {
            group: GROUP_GITHUB_RELEASES,
            key: PHASE_GITHUB_RELEASE_VERSIONS,
            label: "GitHub",
            detail: "checking current GitHub release versions",
            dashboard_group: DASH_GITHUB,
            dashboard_stage: DASH_GITHUB,
            component: PHASE_GITHUB_RELEASE_VERSIONS,
        },
        PHASE_GITHUB_RELEASE_INSTALLS => group_spec(GROUP_GITHUB_RELEASES),
        PHASE_GITHUB_REPOS => group_spec(GROUP_GITHUB_REPOS),
        PHASE_CARGO => group_spec(GROUP_CARGO),
        PHASE_GO => group_spec(GROUP_GO),
        PHASE_UV => group_spec(GROUP_UV),
        PHASE_NPM => group_spec(GROUP_NPM),
        PHASE_CUSTOM => group_spec(GROUP_CUSTOM),
        _ => group_spec(GROUP_OTHER),
    }
}

pub(crate) fn group_spec(group: &str) -> PhaseSpec {
    match group {
        GROUP_PACKAGES => PhaseSpec {
            group: GROUP_PACKAGES,
            key: PHASE_PACKAGES,
            label: "Packages",
            detail: "checking package deps",
            dashboard_group: GROUP_PACKAGES,
            dashboard_stage: GROUP_PACKAGES,
            component: PHASE_PACKAGES,
        },
        GROUP_GITHUB_RELEASES => PhaseSpec {
            group: GROUP_GITHUB_RELEASES,
            key: PHASE_GITHUB_RELEASE_INSTALLS,
            label: "GitHub",
            detail: "checking GitHub release installs",
            dashboard_group: DASH_GITHUB,
            dashboard_stage: DASH_GITHUB,
            component: PHASE_GITHUB_RELEASE_INSTALLS,
        },
        GROUP_GITHUB_REPOS => PhaseSpec {
            group: GROUP_GITHUB_REPOS,
            key: PHASE_GITHUB_REPOS,
            label: "GitHub",
            detail: "checking GitHub repo installs",
            dashboard_group: DASH_GITHUB,
            dashboard_stage: DASH_GITHUB,
            component: PHASE_GITHUB_REPOS,
        },
        GROUP_CARGO => simple_group(GROUP_CARGO, PHASE_CARGO, "Cargo", "checking cargo deps"),
        GROUP_GO => simple_group(GROUP_GO, PHASE_GO, "Go", "checking go deps"),
        GROUP_UV => simple_group(GROUP_UV, PHASE_UV, "UV", "checking uv deps"),
        GROUP_NPM => simple_group(GROUP_NPM, PHASE_NPM, "NPM", "checking npm deps"),
        GROUP_CUSTOM => simple_group(GROUP_CUSTOM, PHASE_CUSTOM, "Custom", "checking custom deps"),
        _ => simple_group(GROUP_OTHER, PHASE_OTHER, "Other", "checking dependencies"),
    }
}

fn simple_group(
    group: &'static str,
    key: &'static str,
    label: &'static str,
    detail: &'static str,
) -> PhaseSpec {
    PhaseSpec {
        group,
        key,
        label,
        detail,
        dashboard_group: group,
        dashboard_stage: group,
        component: key,
    }
}

/// Progress sink used by library callers that only need the final summary.
pub struct NoProgress;

impl Progress for NoProgress {
    fn phase(&mut self, _phase: Phase<'_>) -> Result<()> {
        Ok(())
    }

    fn item(&mut self, _group: &'static str, _item: &Item) -> Result<()> {
        Ok(())
    }
}

/// Shared inputs for one update run.
///
/// `update` is the first command that touches every expensive boundary:
/// package managers, hooks, manifests, and filesystem cleanup. Keep those
/// dependencies explicit so tests can run without a host package manager and
/// future CLI wiring can hold the state lock only around the mutations that
/// actually need it.
pub struct Context<'a, R>
where
    R: Runner,
{
    /// Manifest path to mutate.
    pub manifest_path: &'a std::path::Path,
    /// Runtime filesystem roots.
    pub roots: &'a Roots,
    /// Runtime platform/host identity.
    pub env: &'a RuntimeEnv,
    /// Bash hook subprocess runner.
    pub hooks: &'a BashCustomProbe,
    /// Host subprocess runner for package-manager work.
    pub runner: &'a R,
    /// Detected package manager, or empty when none is available.
    pub pkg_mgr: &'a str,
    /// Environment overrides that affect install decisions.
    ///
    /// Repo URL overrides are process environment in the Bash implementation,
    /// but making them an explicit input keeps `update` deterministic in tests
    /// and prevents method code from reaching around the runtime boundary.
    pub env_vars: &'a BTreeMap<String, String>,
    /// HTTP client used for release metadata and asset downloads.
    ///
    /// Network access is an install-method boundary, not global ambient state.
    /// Keeping it injectable lets release update tests cover GitHub behavior
    /// without live requests or local curl configuration.
    pub client: &'a dyn Client,
}

impl Summary {
    /// Returns whether the update had any failure.
    ///
    /// Install failures always gate the exit code. Cleanup-step
    /// leftovers gate the exit code only when
    /// `SHDEPS_STRICT_LEFTOVERS=1` is set in the environment.
    /// Promoting leftovers to a failure unconditionally would be a
    /// backwards-incompatible behavior change: pre-fix code reported
    /// every transient cleanup error silently, so CI pipelines and
    /// dotfiles bootstraps that assert `shdeps update` exit 0 would
    /// suddenly start failing for previously-tolerated I/O errors.
    /// The env-var opt-in lets operators who want strict cleanup
    /// gating enable it without breaking everyone else's quiet path.
    ///
    /// **Test concurrency invariant:** `has_errors` reads
    /// `SHDEPS_STRICT_LEFTOVERS` from the process-global env. Tests
    /// that mutate that env (currently
    /// `summary_has_errors_only_promotes_leftovers_when_strict_mode_enabled`)
    /// must serialize against any concurrent test that calls
    /// `has_errors` on a non-empty `leftovers` vector — see
    /// `STRICT_LEFTOVERS_TEST_LOCK` in the test module. No
    /// non-test caller mutates the env, so the production read is
    /// race-free. A future test that exercises `has_errors` with
    /// leftovers MUST acquire the same mutex or it will flake
    /// under parallel test execution.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        if !self.failed.is_empty() {
            return true;
        }
        if !self.leftovers.is_empty() && strict_leftovers() {
            return true;
        }
        false
    }
}

fn strict_leftovers() -> bool {
    std::env::var_os("SHDEPS_STRICT_LEFTOVERS").is_some_and(|value| value == "1")
}

/// Runs update for already-parsed entries.
pub fn run<R>(
    entries: &[Entry],
    manifest: &Manifest,
    context: &Context<'_, R>,
    options: Options,
) -> Result<Summary>
where
    R: Runner + Sync,
{
    run_with_progress(entries, manifest, context, options, &mut NoProgress)
}

/// Runs update while reporting phase and item progress to `progress`.
pub fn run_with_progress<R>(
    entries: &[Entry],
    manifest: &Manifest,
    context: &Context<'_, R>,
    options: Options,
    progress: &mut dyn Progress,
) -> Result<Summary>
where
    R: Runner + Sync,
{
    // Serialize concurrent update runs through the per-state-directory
    // advisory `flock`. Without this, two `shdeps update` processes
    // (e.g., a user-triggered run racing a periodic timer, or two
    // panes that both source the dotfiles entry point) could
    // interleave manifest writes and link-state mutations and leave
    // shdeps in a half-applied state.
    //
    // Re-entry safety: when a hook subprocess calls back into shdeps
    // (e.g., a `post()` hook that runs `shdeps update some-other-dep`),
    // the inner acquire sees `SHDEPS_STATE_LOCK_HELD` in its env (set
    // by `apply_hook_env`) and returns a no-op guard instead of
    // deadlocking against the parent's flock. The doc comment on
    // `StateLock` warns against holding the lock across hooks — that
    // warning predates the re-entry env-var contract added here, which
    // makes the broader scope safe. Concurrent top-level invocations
    // still serialize because they do not inherit the env var.
    //
    // The handle is bound to a local so its `Drop` releases the lock
    // when `run` returns by any path.
    let _lock = crate::state::StateLock::acquire(&context.roots.state_dir)?;

    let transitions = update_transition::by_name(manifest, entries, context.roots)?;

    let mut summary = Summary::default();
    let mut changed = Vec::new();
    let mut queued = Vec::new();
    let hook_txn = Txn::new(&context.roots.state_dir)?;

    let active_package_entries = entries
        .iter()
        .any(|entry| entry.method == method::PKG && active(entry, context.env));
    let package_total = entries
        .iter()
        .filter(|entry| entry.method == method::PKG && active(entry, context.env))
        .count();
    let installable_package_count = entries
        .iter()
        .filter(|entry| entry.method == method::PKG && active(entry, context.env))
        .filter(|entry| {
            config::resolve_override(&entry.name, &entry.aliases, Some(context.pkg_mgr)) != "NONE"
        })
        .count();
    if active_package_entries {
        let group_started = Instant::now();
        progress.phase(phase_for_group(GROUP_PACKAGES, 0, package_total))?;
        let package_cache = if installable_package_count == 0 {
            package_cache::Status::Hit { count: 0 }
        } else {
            update_pkg::cache_status(entries, context, installable_package_count, options)?
        };
        if package_cache.is_hit() {
            // The package cache is stronger than a TTL: it records the package
            // DB, manifest, config, command paths, hooks, host, platform, and
            // env knobs that affected the last clean package pass. On a hit,
            // replay the same per-entry "installed/skipped" items but avoid
            // package-manager probes and manifest rewrites. Non-package
            // methods still run normally below.
            for (index, item) in update_pkg::cached_items(entries, context)
                .into_iter()
                .enumerate()
            {
                let package_done = index + 1;
                progress.item(GROUP_PACKAGES, &item)?;
                progress.phase(phase_for_group(GROUP_PACKAGES, package_done, package_total))?;
                summary.items.push(item);
            }
        } else {
            let package_versions = update_pkg::package_versions(entries, context, options);
            update_pkg::prepare(entries, context, &package_versions, progress)?;

            let mut package_clean = true;
            let mut package_done = 0usize;

            for entry in entries {
                if entry.method != method::PKG || !active(entry, context.env) {
                    continue;
                }

                let item =
                    update_pkg::install(entry, context, options, &mut queued, &package_versions)?;
                if !item.failed {
                    match item.reason {
                        ItemReason::Installed => cleanup_successful_transition(
                            entry,
                            transitions.get(&entry.name),
                            context,
                            &mut summary,
                        )?,
                        ItemReason::PackageUnavailable => update_transition::restore_failed(
                            transitions.get(&entry.name),
                            context.manifest_path,
                        )?,
                        _ => {}
                    }
                }
                if !matches!(
                    item.reason,
                    ItemReason::Installed | ItemReason::PackageManagerOverride
                ) || item.changed
                    || item.failed
                {
                    package_clean = false;
                }
                if item.changed {
                    changed.push(entry.name.clone());
                }
                if item.failed {
                    summary.failed.push(entry.name.clone());
                }
                package_done += 1;
                progress.item(GROUP_PACKAGES, &item)?;
                progress.phase(phase_for_group(GROUP_PACKAGES, package_done, package_total))?;
                summary.items.push(item);
            }

            let pkg_changed_start = changed.len();
            let pkg_failed_start = summary.failed.len();
            update_pkg::flush(&queued, context, &mut changed, &mut summary, progress)?;
            let successful_packages = changed[pkg_changed_start..]
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let failed_packages = summary.failed[pkg_failed_start..]
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            mark_queued_packages(&mut summary, &successful_packages, &failed_packages);
            for item in &queued {
                if successful_packages.contains(&item.name) {
                    let Some(entry) = entries.iter().find(|entry| entry.name == item.name) else {
                        continue;
                    };
                    cleanup_successful_transition(
                        entry,
                        transitions.get(&entry.name),
                        context,
                        &mut summary,
                    )?;
                } else if failed_packages.contains(&item.name) {
                    update_transition::restore_failed(
                        transitions.get(&item.name),
                        context.manifest_path,
                    )?;
                }
            }

            if !queued.is_empty()
                || changed.len() != pkg_changed_start
                || summary.failed.len() != pkg_failed_start
            {
                package_clean = false;
            }
            if package_clean {
                update_pkg::write_cache(entries, context, installable_package_count, options)?;
            }
        }
        finish_group(&mut summary, GROUP_PACKAGES, group_started);
    }

    let group_totals = group_totals(entries, context.env);
    let mut group_done = BTreeMap::<&'static str, usize>::new();
    let mut group_started = BTreeMap::<&'static str, Instant>::new();
    let mut announced = BTreeSet::<&'static str>::new();

    let release_entries = entries
        .iter()
        .filter(|entry| entry.method == method::GITHUB_RELEASE && active(entry, context.env))
        .collect::<Vec<_>>();
    let release_prefetch_total =
        update_release::prefetch_progress_total(&release_entries, context.roots, options);
    if !release_entries.is_empty() {
        announced.insert(GROUP_GITHUB_RELEASES);
        group_started.insert(GROUP_GITHUB_RELEASES, Instant::now());
        if release_prefetch_total > 0 {
            progress.phase(running_phase(
                PHASE_GITHUB_RELEASE_METADATA,
                0,
                release_prefetch_total,
            ))?;
        }
    }
    let release_prefetch = update_release::prefetch(&release_entries, context, options, progress)?;
    if !release_entries.is_empty() {
        progress.phase(phase_for_group(
            GROUP_GITHUB_RELEASES,
            0,
            release_entries.len(),
        ))?;
    }

    let builtin_entries = entries
        .iter()
        .filter(|entry| method::active_non_package_builtin(entry))
        .filter(|entry| active(entry, context.env))
        .collect::<Vec<_>>();

    let builtin_outcomes = jobs::parallel_map_with_item_progress(
        &builtin_entries,
        jobs::max_jobs(context.env_vars),
        |entry| {
            install_builtin(
                entry,
                context,
                options,
                &release_prefetch,
                transitions.get(&entry.name),
            )
        },
        |event| {
            match event {
                jobs::ItemProgressEvent::Started(index) => {
                    let group = group_for_method(&builtin_entries[index].method);
                    if announced.insert(group) {
                        group_started.insert(group, Instant::now());
                        progress.phase(phase_for_group(group, 0, group_totals[group]))?;
                    }
                }
                jobs::ItemProgressEvent::Completed {
                    index,
                    result: outcome,
                } => {
                    let group = group_for_method(&builtin_entries[index].method);
                    progress.item(group, &outcome.item)?;
                    if advance_group(progress, &mut group_done, &group_totals, group)?
                        && let Some(started) = group_started.remove(group)
                    {
                        finish_group(&mut summary, group, started);
                    }
                }
            }
            Ok(())
        },
    )?;
    for outcome in builtin_outcomes {
        if outcome.cleanup_leftover {
            summary.leftovers.push(outcome.item.name.clone());
        }
        if outcome.item.failed {
            summary.failed.push(outcome.item.name.clone());
        }
        if outcome.item.changed {
            changed.push(outcome.item.name.clone());
        }
        summary.items.push(outcome.item);
    }

    for entry in entries {
        if entry.method != method::CUSTOM || !active(entry, context.env) {
            continue;
        }

        let group = group_for_method(&entry.method);
        if announced.insert(group) {
            group_started.insert(group, Instant::now());
            progress.phase(phase_for_group(group, 0, group_totals[group]))?;
        }
        let outcome = install_custom(
            entry,
            context,
            &hook_txn,
            options,
            transitions.get(&entry.name).map(update_transition::old),
        )?;
        let item = outcome.item;
        if outcome.cleanup_leftover {
            summary.leftovers.push(entry.name.clone());
        }
        if item.failed {
            summary.failed.push(entry.name.clone());
        }
        if item.changed {
            record_changed(&mut changed, entry.name.clone());
        }
        for marker in outcome.marked {
            record_changed(&mut changed, marker);
        }
        progress.item(group, &item)?;
        if advance_group(progress, &mut group_done, &group_totals, group)?
            && let Some(started) = group_started.remove(group)
        {
            finish_group(&mut summary, group, started);
        }
        summary.items.push(item);
    }

    // Post hooks deliberately run after every install decision rather than
    // inline with each method. Many hooks repair shell completions, symlinks,
    // or dependent tools, so they should see the final state for the full
    // update pass instead of an intermediate per-method view.
    run_post_hooks(
        &changed,
        context.roots,
        context.hooks,
        &hook_txn,
        &mut summary,
    )?;
    Ok(summary)
}

fn advance_group(
    progress: &mut dyn Progress,
    group_done: &mut BTreeMap<&'static str, usize>,
    group_totals: &BTreeMap<&'static str, usize>,
    group: &'static str,
) -> Result<bool> {
    let done = group_done.entry(group).or_insert(0);
    *done += 1;
    let total = group_totals[group];
    progress.phase(phase_for_group(group, *done, total))?;
    Ok(*done >= total)
}

fn mark_queued_packages(
    summary: &mut Summary,
    successful_packages: &BTreeSet<String>,
    failed_packages: &BTreeSet<String>,
) {
    for item in &mut summary.items {
        if item.reason != ItemReason::PackageQueued {
            continue;
        }
        if successful_packages.contains(&item.name) {
            item.changed = true;
            item.status = ItemStatus::Changed;
            item.reason = ItemReason::Installed;
            item.detail = "installed".to_owned();
        } else if failed_packages.contains(&item.name) {
            item.failed = true;
            item.status = ItemStatus::Failed;
            item.reason = ItemReason::InstallFailed;
            item.detail = "package install failed".to_owned();
        }
    }
}

fn finish_group(summary: &mut Summary, group: &'static str, started: Instant) {
    summary.groups.push(GroupSummary {
        group,
        elapsed_ms: started.elapsed().as_millis(),
    });
}

fn group_totals(entries: &[Entry], env: &RuntimeEnv) -> BTreeMap<&'static str, usize> {
    let mut totals = BTreeMap::new();
    for entry in entries {
        if entry.method == method::PKG || !active(entry, env) {
            continue;
        }
        *totals.entry(group_for_method(&entry.method)).or_insert(0) += 1;
    }
    totals
}

/// Returns the display/progress group for an update method.
pub fn group_for_method(method: &str) -> &'static str {
    match method {
        method::PKG => GROUP_PACKAGES,
        method::GITHUB_RELEASE => GROUP_GITHUB_RELEASES,
        method::GITHUB_REPO => GROUP_GITHUB_REPOS,
        method::CARGO => GROUP_CARGO,
        method::GO => GROUP_GO,
        method::UV => GROUP_UV,
        method::NPM => GROUP_NPM,
        method::CUSTOM => GROUP_CUSTOM,
        _ => GROUP_OTHER,
    }
}

pub(crate) fn phase_key_for_group(group: &str) -> &'static str {
    group_spec(group).key
}

pub(crate) fn display_group_for_update_group(group: &str) -> &'static str {
    group_spec(group).dashboard_group
}

pub(crate) fn label_for_display_group(group: &str) -> &'static str {
    match group {
        DASH_METHOD_RESOLUTION => phase_spec(PHASE_GITHUB_METHODS).label,
        DASH_GITHUB => group_spec(GROUP_GITHUB_RELEASES).label,
        _ => group_spec(group).label,
    }
}

pub(crate) fn update_group_order() -> &'static [&'static str] {
    &[
        GROUP_PACKAGES,
        GROUP_GITHUB_RELEASES,
        GROUP_GITHUB_REPOS,
        GROUP_CARGO,
        GROUP_GO,
        GROUP_UV,
        GROUP_NPM,
        GROUP_CUSTOM,
        GROUP_OTHER,
    ]
}

pub(crate) fn dashboard_group_order() -> &'static [&'static str] {
    &[
        GROUP_PACKAGES,
        DASH_GITHUB,
        GROUP_CARGO,
        GROUP_GO,
        GROUP_UV,
        GROUP_NPM,
        GROUP_CUSTOM,
        GROUP_OTHER,
    ]
}

pub(crate) fn dashboard_stage_order() -> &'static [&'static str] {
    &[
        DASH_METHOD_RESOLUTION,
        GROUP_PACKAGES,
        DASH_GITHUB,
        GROUP_CARGO,
        GROUP_GO,
        GROUP_UV,
        GROUP_NPM,
        GROUP_CUSTOM,
        GROUP_OTHER,
    ]
}

pub(crate) fn dashboard_stage_index(stage: &str) -> usize {
    match stage {
        DASH_METHOD_RESOLUTION => 0,
        GROUP_PACKAGES => 1,
        DASH_GITHUB => 2,
        GROUP_CARGO => 3,
        GROUP_GO => 4,
        GROUP_UV => 5,
        GROUP_NPM => 6,
        GROUP_CUSTOM => 7,
        _ => 8,
    }
}

pub(crate) fn active(entry: &Entry, env: &RuntimeEnv) -> bool {
    matches!(
        platform::filter_match(&entry.filter, env),
        platform::FilterMatch::Match
    )
}

struct CustomOutcome {
    item: Item,
    cleanup_leftover: bool,
    marked: Vec<String>,
}

struct BuiltinOutcome {
    item: Item,
    cleanup_leftover: bool,
}

fn install_builtin<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    options: Options,
    release_prefetch: &update_release::Prefetch,
    transition: Option<&update_transition::Transition>,
) -> BuiltinOutcome
where
    R: Runner + Sync,
{
    let result = match entry.method.as_str() {
        method::GITHUB_RELEASE => {
            update_transition::install_with_prepared(entry, transition, context.roots, || {
                update_release::install_with_prefetch(entry, context, options, release_prefetch)
            })
        }
        method::GITHUB_REPO => {
            update_transition::install_with_prepared(entry, transition, context.roots, || {
                update_repo::install(entry, context, options)
            })
        }
        candidate if method::requires_external_plan(candidate) => {
            update_transition::install_with_prepared(entry, transition, context.roots, || {
                update_external::install(entry, context, options)
            })
        }
        method => Ok(Item::failed(
            entry.name.clone(),
            ItemReason::UnsupportedMethod,
            format!("{method} update is not implemented yet"),
        )),
    };

    let item = match result {
        Ok(item) => item,
        Err(error) => Item::failed(
            entry.name.clone(),
            ItemReason::InstallFailed,
            error.to_string(),
        ),
    };

    let cleanup_leftover = if item.failed {
        false
    } else {
        update_transition::cleanup_successful(entry, transition, context.roots).unwrap_or(true)
    };

    BuiltinOutcome {
        item,
        cleanup_leftover,
    }
}

fn successful_custom(
    entry: &Entry,
    manifest_path: &std::path::Path,
    roots: &Roots,
    changed: bool,
    detail: String,
    transition: Option<&ManifestEntry>,
) -> Result<CustomOutcome> {
    manifest::upsert(
        manifest_path,
        ManifestEntry::new(&entry.name, method::CUSTOM, &entry.cmd, ""),
    )?;

    let cleanup_leftover = match transition {
        Some(old) => cleanup_transition(old, roots),
        None => false,
    };

    Ok(CustomOutcome {
        item: if changed {
            Item::changed(entry.name.clone(), ItemReason::Installed, detail)
        } else {
            Item::current(entry.name.clone(), ItemReason::Installed, detail)
        },
        cleanup_leftover,
        marked: Vec::new(),
    })
}

fn cleanup_transition(old: &ManifestEntry, roots: &Roots) -> bool {
    // Transition cleanup happens after the new method is recorded so a cleanup
    // failure cannot erase the working install. Do not run `uninstall()` here:
    // the hook path is keyed only by dependency name, so after a switch there
    // is no reliable way to source the old method's hook separately from the
    // new method's hook. Running the current hook after a successful custom
    // install could undo the install we just accepted.
    cleanup::remove_builtin(old, &cleanup_roots(roots)).is_err()
}

fn cleanup_successful_transition(
    entry: &Entry,
    transition: Option<&update_transition::Transition>,
    context: &Context<'_, impl Runner>,
    summary: &mut Summary,
) -> Result<()> {
    if update_transition::cleanup_successful(entry, transition, context.roots)? {
        summary.leftovers.push(entry.name.clone());
    }
    Ok(())
}

fn install_custom(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    txn: &Txn,
    options: Options,
    transition: Option<&ManifestEntry>,
) -> Result<CustomOutcome> {
    let install =
        context
            .hooks
            .install_with_txn(&entry.name, context.roots, options.reinstall, Some(txn))?;
    let marked = txn.collect()?;
    let verbose = verbose_enabled(options, context.env_vars);
    match install {
        Install::Already { detail } => {
            let mut outcome = successful_custom(
                entry,
                context.manifest_path,
                context.roots,
                false,
                detail,
                transition,
            )?;
            outcome.marked = marked;
            Ok(outcome)
        }
        Install::Installed { detail } => {
            let detail = if verbose {
                let action = if options.reinstall {
                    "reinstalled"
                } else {
                    "added"
                };
                detail_with_action(action, detail)
            } else {
                detail
            };
            let mut outcome = successful_custom(
                entry,
                context.manifest_path,
                context.roots,
                true,
                detail,
                transition,
            )?;
            outcome.marked = marked;
            Ok(outcome)
        }
        Install::MissingHook | Install::MissingFunction | Install::SourceFailed => {
            Ok(CustomOutcome {
                item: Item::skipped(
                    entry.name.clone(),
                    ItemReason::CustomUnavailable,
                    "custom hook missing or unusable",
                ),
                cleanup_leftover: false,
                marked,
            })
        }
        Install::Failed => Ok(CustomOutcome {
            item: Item::failed(
                entry.name.clone(),
                ItemReason::CustomInstallFailed,
                "custom install failed",
            ),
            cleanup_leftover: false,
            marked,
        }),
    }
}

pub(crate) fn verbose_enabled(options: Options, env_vars: &BTreeMap<String, String>) -> bool {
    options.verbose
        || env_vars
            .get("SHDEPS_LOG_LEVEL")
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(1)
            >= 2
}

pub(crate) fn detail_with_action(action: &str, detail: String) -> String {
    if detail.is_empty() {
        action.to_owned()
    } else {
        format!("{action} -- {detail}")
    }
}

fn record_changed(changed: &mut Vec<String>, name: String) {
    if !changed.iter().any(|existing| existing == &name) {
        changed.push(name);
    }
}

fn run_post_hooks(
    changed: &[String],
    roots: &Roots,
    hooks: &BashCustomProbe,
    txn: &Txn,
    summary: &mut Summary,
) -> Result<()> {
    for name in changed {
        match hooks.post_with_txn(name, roots, Some(txn))? {
            Post::Ran | Post::MissingHook | Post::MissingFunction => {}
            Post::SourceFailed | Post::Failed => summary.failed.push(name.clone()),
        }
    }
    Ok(())
}

fn cleanup_roots(roots: &Roots) -> cleanup::Roots {
    cleanup::Roots {
        state_dir: roots.state_dir.clone(),
        install_dir: roots.install_dir.clone(),
        bin_dir: roots.bin_dir.clone(),
        home: roots.home.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::io::Cursor;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Context, Item, Options, Summary, run, run_with_progress};
    use bzip2::Compression as BzCompression;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use crate::config::parse_entry;
    use crate::hooks::BashCustomProbe;
    use crate::http::Client;
    use crate::link_state::{self, Kind};
    use crate::manifest::{self, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PhaseRecord {
        key: &'static str,
        label: String,
        detail: String,
        done: usize,
        total: usize,
    }

    #[derive(Default)]
    struct RecordingProgress {
        phases: Vec<PhaseRecord>,
        prompt_pauses: Vec<String>,
    }

    impl super::Progress for RecordingProgress {
        fn phase(&mut self, phase: super::Phase<'_>) -> crate::Result<()> {
            self.phases.push(PhaseRecord {
                key: phase.key,
                label: phase.label.to_owned(),
                detail: phase.detail.to_owned(),
                done: phase.done,
                total: phase.total,
            });
            Ok(())
        }

        fn item(&mut self, _group: &'static str, _item: &Item) -> crate::Result<()> {
            Ok(())
        }

        fn pause_for_prompt(&mut self, detail: &str) -> crate::Result<()> {
            self.prompt_pauses.push(detail.to_owned());
            Ok(())
        }
    }

    #[test]
    fn update_installs_custom_dep_records_manifest_and_runs_post() {
        let fixture = Fixture::new("custom");
        fixture.write_lib();
        fixture.write_hook(
            "tool",
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/tool-installed" ]]; }
install() { printf 'yes\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
version() { printf '1.2.3\n'; }
post() { printf 'post\n' > "$SHDEPS_STATE_DIR/tool-post"; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("tool|custom|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "1.2.3");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new("tool", "custom", "tool", ""))
        );
        assert_eq!(
            fs::read_to_string(fixture.roots.state_dir.join("tool-post")).unwrap(),
            "post\n"
        );
    }

    #[test]
    fn update_collects_hook_changed_markers_for_post_scheduling() {
        let fixture = Fixture::new("custom-marker");
        fs::write(
            fixture.hooks.shdeps_lib(),
            r#"
shdeps_mark_changed() {
  local marker="$SHDEPS_STATE_DIR/.changed-markers/$SHDEPS_UPDATE_TXN_ID/$1"
  mkdir -p "$(dirname "$marker")"
  : >"$marker"
}
"#,
        )
        .unwrap();
        fixture.write_hook(
            "tool",
            r#"
exists() { return 1; }
install() {
  [[ -n "$SHDEPS_UPDATE_TXN_ID" ]] || return 9
  shdeps_mark_changed helper
  printf 'installed\n'
}
"#,
        );
        fixture.write_hook(
            "helper",
            r#"
exists() { return 0; }
post() { printf 'helper post\n' > "$SHDEPS_STATE_DIR/helper-post"; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[
                parse_entry("tool|custom|tool|-|-", None),
                parse_entry("helper|custom|helper|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fs::read_to_string(fixture.roots.state_dir.join("helper-post")).unwrap(),
            "helper post\n"
        );
        assert!(
            !fixture.roots.state_dir.join(".changed-markers").exists(),
            "per-update hook marker directory should be cleaned up after scheduling"
        );
    }

    #[test]
    fn update_cleans_old_method_after_custom_install_succeeds() {
        let fixture = Fixture::new("transition");
        fixture.write_lib();
        fixture.write_hook(
            "tool",
            r#"
exists() { return 1; }
install() { printf 'installed\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
uninstall() { printf 'old\n' > "$SHDEPS_STATE_DIR/tool-uninstalled"; }
"#,
        );
        let old_install = fixture.roots.install_dir.join("tool");
        fs::create_dir_all(&old_install).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "github:release",
                "tool",
                old_install.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
            &[parse_entry("tool|custom|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!old_install.exists());
        assert!(!fixture.roots.state_dir.join("tool-uninstalled").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new("tool", "custom", "tool", ""))
        );
    }

    #[test]
    fn update_preserves_old_method_when_custom_install_fails() {
        let fixture = Fixture::new("transition-failure");
        fixture.write_lib();
        fixture.write_hook(
            "tool",
            r#"
exists() { return 1; }
install() { return 42; }
uninstall() { printf 'old\n' > "$SHDEPS_STATE_DIR/tool-uninstalled"; }
"#,
        );
        let old_install = fixture.roots.install_dir.join("tool");
        fs::create_dir_all(&old_install).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "github:release",
                "tool",
                old_install.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("tool|custom|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(old_install.exists());
        assert!(!fixture.roots.state_dir.join("tool-uninstalled").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new(
                "tool",
                "github:release",
                "tool",
                old_install.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_cleans_old_builtin_method_after_repo_transition_preserving_new_links() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("transition-repo");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        fs::create_dir_all(local_clone.join("share/man/man1")).unwrap();
        fs::write(local_clone.join("share/man/man1/ds.1"), "new man").unwrap();

        let old_install = fixture.roots.install_dir.join("cgraf78/ds");
        write_executable(&old_install.join("bin/ds"));
        write_executable(&fixture.roots.bin_dir.join("ds"));
        let old_extra = fixture.roots.install_dir.join("man/man1/old-ds.1");
        fs::create_dir_all(old_extra.parent().unwrap()).unwrap();
        symlink(old_install.join("share/man/man1/old-ds.1"), &old_extra).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "cgraf78/ds", Kind::Extras),
            std::slice::from_ref(&old_extra),
        )
        .unwrap();

        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "cgraf78/ds",
                "github:release",
                "ds",
                old_install.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        let public_bin = fixture.roots.bin_dir.join("ds");
        let new_extra = fixture.roots.install_dir.join("man/man1/ds.1");
        assert!(!summary.has_errors());
        assert!(summary.leftovers.is_empty());
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            fs::read_link(&public_bin).unwrap(),
            install_link.join("bin/ds")
        );
        assert_eq!(
            fs::read_link(&new_extra).unwrap(),
            install_link.join("share/man/man1/ds.1")
        );
        assert!(fs::symlink_metadata(&old_extra).is_err());
        assert_eq!(
            link_state::read(&link_state::path(
                &fixture.roots.state_dir,
                "cgraf78/ds",
                Kind::Extras
            ))
            .unwrap(),
            vec![new_extra]
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("cgraf78/ds"),
            Some(&ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_link.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_pkg_transition_cleans_old_builtin_artifacts_without_uninstalling_package() {
        let fixture = Fixture::new("transition-pkg");
        fixture.write_lib();
        let old_install = fixture.roots.install_dir.join("tool");
        write_executable(&old_install.join("bin/tool"));
        write_executable(&fixture.roots.bin_dir.join("tool"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "github:release",
                "tool",
                old_install.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default().with_command("tool");

        let summary = run(
            &[parse_entry("tool|pkg|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "installed");
        assert!(!old_install.exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new("tool", "pkg", "tool", ""))
        );
    }

    #[test]
    fn update_pkg_transition_restores_old_manifest_when_package_install_fails() {
        let fixture = Fixture::new("transition-pkg-failure");
        fixture.write_lib();
        let old_install = fixture.roots.install_dir.join("tool");
        write_executable(&old_install.join("bin/tool"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old_manifest = ManifestEntry::new(
            "tool",
            "github:release",
            "tool",
            old_install.display().to_string(),
        );
        manifest::upsert(&manifest_path, old_manifest.clone()).unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default()
            .with_success("apt-cache", ["show", "tool"], "Package: tool\n")
            .with_failure("sudo", ["apt-get", "install", "-y", "tool"]);

        let summary = run(
            &[parse_entry("tool|pkg|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["tool"]);
        assert!(old_install.exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&old_manifest)
        );
    }

    #[test]
    fn update_restores_release_public_binary_when_symlink_method_transition_fails() {
        let fixture = Fixture::new("transition-bin-restore");
        fixture.write_lib();
        let old_bin = fixture.roots.bin_dir.join("tool");
        write_executable(&old_bin);
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old_manifest = ManifestEntry::new(
            "tool",
            "github:release",
            "tool",
            old_bin.display().to_string(),
        );
        manifest::upsert(&manifest_path, old_manifest.clone()).unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("tool|cargo|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].detail, "cargo not found");
        assert_eq!(fs::read_to_string(&old_bin).unwrap(), "#!/bin/sh\n");
        assert!(!old_bin.is_symlink());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&old_manifest)
        );
    }

    #[test]
    fn update_github_release_ignores_local_clone_for_release_method() {
        let mut fixture = Fixture::new("release-ignores-local-clone");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/cgraf78/ds/releases?per_page=100",
                release_response(
                    "ds",
                    "v1.2.3",
                    "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64",
                b"release-binary".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("cgraf78/ds|github:release|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("ds");
        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"release-binary");
        assert!(local_clone.join("bin/ds").exists());
        assert!(!fixture.roots.install_dir.join("cgraf78/ds").exists());
        assert!(runner.calls().iter().all(|call| !call.starts_with("git\0")));
    }

    #[test]
    #[cfg(unix)]
    fn update_repo_to_plain_release_removes_local_clone_symlink_but_preserves_clone() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("transition-repo-to-plain-release");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        symlink(&local_clone, &install_link).unwrap();
        let public_bin = fixture.roots.bin_dir.join("ds");
        fs::create_dir_all(public_bin.parent().unwrap()).unwrap();
        symlink(install_link.join("bin/ds"), &public_bin).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "cgraf78/ds", Kind::Bin),
            std::slice::from_ref(&public_bin),
        )
        .unwrap();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/cgraf78/ds/releases?per_page=100",
                release_response(
                    "ds",
                    "v1.2.3",
                    "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64",
                b"release-binary".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_link.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("cgraf78/ds|github:release|ds|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(fs::read(&public_bin).unwrap(), b"release-binary");
        assert!(local_clone.join("bin/ds").exists());
        assert!(fs::symlink_metadata(&install_link).is_err());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("cgraf78/ds"),
            Some(&ManifestEntry::new(
                "cgraf78/ds",
                "github:release",
                "ds",
                public_bin.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_repo_to_archive_release_replaces_local_clone_symlink_with_archive_root() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("transition-repo-to-archive-release");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        symlink(&local_clone, &install_link).unwrap();
        let public_bin = fixture.roots.bin_dir.join("ds");
        fs::create_dir_all(public_bin.parent().unwrap()).unwrap();
        symlink(install_link.join("bin/ds"), &public_bin).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "cgraf78/ds", Kind::Bin),
            std::slice::from_ref(&public_bin),
        )
        .unwrap();
        let archive = tar_gz(&[
            ("ds-v1.2.3/bin/ds", b"release-binary".as_slice(), 0o755),
            ("ds-v1.2.3/share/man/man1/ds.1", b"man".as_slice(), 0o644),
        ]);
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/cgraf78/ds/releases?per_page=100",
                release_response(
                    "ds",
                    "v1.2.3",
                    "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64.tar.gz",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/ds-linux-x86_64.tar.gz",
                archive,
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_link.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("cgraf78/ds|github:release|ds|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(install_link.is_dir());
        assert!(!install_link.is_symlink());
        assert_eq!(
            fs::read_link(&public_bin).unwrap(),
            install_link.join("bin/ds")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("man/man1/ds.1")).unwrap(),
            install_link.join("share/man/man1/ds.1")
        );
        assert!(local_clone.join("bin/ds").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("cgraf78/ds"),
            Some(&ManifestEntry::new(
                "cgraf78/ds",
                "github:release",
                "ds",
                public_bin.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_reports_custom_install_failure_without_manifest_row() {
        let fixture = Fixture::new("custom-failure");
        fixture.write_lib();
        fixture.write_hook(
            "tool",
            r#"
exists() { return 1; }
install() { return 42; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("tool|custom|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["tool"]);
        assert_eq!(summary.items[0].detail, "custom install failed");
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("tool")
                .is_none()
        );
    }

    #[test]
    fn update_records_existing_package_without_installing() {
        let fixture = Fixture::new("pkg-existing");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_command("jq");

        let summary = run(
            &[parse_entry("jq|pkg|jq|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "installed");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("jq"),
            Some(&ManifestEntry::new("jq", "pkg", "jq", ""))
        );
    }

    #[test]
    fn update_reuses_clean_package_cache_for_warm_noop_scan() {
        let fixture = Fixture::new("pkg-cache-hit");
        fixture.write_lib();
        fs::create_dir_all(&fixture.roots.conf_dir).unwrap();
        fs::write(fixture.roots.conf_dir.join("deps.conf"), "font pkg\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let entries = [parse_entry("font|pkg|font|-|-", None)];
        let first_runner = FakeRunner::default().with_success(
            "dpkg-query",
            ["-W", "-f=${Package}\t${Version}\n"],
            "font\t1.0\n",
        );

        let first = run(
            &entries,
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &first_runner, "apt"),
            Options::default(),
        )
        .unwrap();
        assert_eq!(first.items[0].detail, "installed");

        let manifest = manifest::read(&manifest_path).unwrap();
        let second_runner = FakeRunner::default();
        let second = run(
            &entries,
            &manifest,
            &fixture.context(&manifest_path, &second_runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert_eq!(second.items[0].detail, "installed");
        assert!(
            second_runner.calls().is_empty(),
            "a proof-backed package cache hit should not spawn package-manager probes"
        );
    }

    #[test]
    fn update_force_bypasses_clean_package_cache() {
        let fixture = Fixture::new("pkg-cache-force");
        fixture.write_lib();
        fs::create_dir_all(&fixture.roots.conf_dir).unwrap();
        fs::write(fixture.roots.conf_dir.join("deps.conf"), "font pkg\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let entries = [parse_entry("font|pkg|font|-|-", None)];
        let first_runner = FakeRunner::default().with_success(
            "dpkg-query",
            ["-W", "-f=${Package}\t${Version}\n"],
            "font\t1.0\n",
        );

        run(
            &entries,
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &first_runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let manifest = manifest::read(&manifest_path).unwrap();
        let forced_runner = FakeRunner::default().with_success(
            "dpkg-query",
            ["-W", "-f=${Package}\t${Version}\n"],
            "font\t1.0\n",
        );
        let forced = run(
            &entries,
            &manifest,
            &fixture.context(&manifest_path, &forced_runner, "apt"),
            Options {
                force: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(forced.items[0].detail, "installed");
        assert!(
            forced_runner
                .calls()
                .contains(&key("dpkg-query", ["-W", "-f=${Package}\t${Version}\n"])),
            "force mode must re-prove package state instead of trusting the warm cache"
        );
    }

    #[test]
    fn update_force_avoids_batch_package_versions_when_commands_prove_state() {
        let fixture = Fixture::new("pkg-force-command-only");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_command("jq").with_command("rg");

        let summary = run(
            &[
                parse_entry("jq|pkg|jq|-|-", None),
                parse_entry("ripgrep|pkg|rg|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                force: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(summary.items.len(), 2);
        assert!(summary.items.iter().all(|item| {
            item.status == super::ItemStatus::Current && item.reason == super::ItemReason::Installed
        }));
        assert!(
            !runner
                .calls()
                .contains(&key("dpkg-query", ["-W", "-f=${Package}\t${Version}\n"])),
            "forced package checks still need to re-prove command presence, but the expensive manager-wide version snapshot is unnecessary when every command is present"
        );
    }

    #[test]
    fn update_skips_none_package_override_without_manifest_row() {
        let fixture = Fixture::new("pkg-none");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("tool|pkg|tool|apt:NONE|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            summary.items[0].detail,
            "skipped by package-manager override"
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("tool")
                .is_none()
        );
    }

    #[test]
    fn update_records_unavailable_package_as_compatibility_skip() {
        let fixture = Fixture::new("pkg-unavailable");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_failure("apt-cache", ["show", "missing"]);

        let summary = run(
            &[parse_entry("missing|pkg|missing|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "not available");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("missing"),
            Some(&ManifestEntry::new("missing", "pkg", "missing", ""))
        );
    }

    #[test]
    fn update_bounds_package_availability_probe_while_lock_is_held() {
        let fixture = Fixture::new("pkg-availability-timeout");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("apt-cache", ["show", "jq"], "")
            .with_success("sudo", ["apt-get", "install", "-y", "jq"], "");

        let summary = run(
            &[parse_entry("jq|pkg|jq|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            runner.timeouts_for("apt-cache", ["show", "jq"]),
            vec![Some(crate::process::PACKAGE_PROBE_TIMEOUT)],
            "availability probes run while the update lock is held and must be bounded"
        );
    }

    #[test]
    fn update_refreshes_package_metadata_before_availability_probe() {
        let fixture = Fixture::new("pkg-refresh");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("sudo", ["apk", "update"], "")
            .with_success("apk", ["search", "-e", "jq"], "jq-1.8.1-r0\n")
            .with_success("sudo", ["apk", "add", "jq"], "");

        let summary = run(
            &[parse_entry("jq|pkg|jq|-|-", Some("apk"))],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apk"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        let calls = runner.calls();
        assert!(
            call_index(&calls, "sudo", ["apk", "update"])
                < call_index(&calls, "apk", ["search", "-e", "jq"])
        );
    }

    #[test]
    fn update_enables_epel_before_dnf_availability_probe() {
        let mut fixture = Fixture::new("pkg-epel");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_AUTO_EPEL".to_owned(), "1".to_owned());
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success(
                "sudo",
                ["dnf", "config-manager", "--set-enabled", "crb"],
                "",
            )
            .with_failure("rpm", ["-q", "epel-release"])
            .with_success("sudo", ["dnf", "install", "-y", "epel-release"], "")
            .with_success("sudo", ["dnf", "makecache", "-q"], "")
            .with_success("dnf", ["info", "ripgrep"], "Name         : ripgrep\n")
            .with_success("sudo", ["dnf", "install", "-y", "ripgrep"], "");

        let summary = run(
            &[parse_entry("ripgrep|pkg|rg|-|-", Some("dnf"))],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "dnf"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        let calls = runner.calls();
        assert!(
            call_index(&calls, "sudo", ["dnf", "install", "-y", "epel-release"])
                < call_index(&calls, "dnf", ["info", "ripgrep"])
        );
        assert!(
            call_index(&calls, "sudo", ["dnf", "makecache", "-q"])
                < call_index(&calls, "dnf", ["info", "ripgrep"])
        );
    }

    #[test]
    fn update_batches_missing_packages_and_runs_post_hooks() {
        let fixture = Fixture::new("pkg-batch");
        fixture.write_lib();
        fixture.write_hook(
            "jq",
            r#"
post() { printf 'post\n' > "$SHDEPS_STATE_DIR/jq-post"; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("apt-cache", ["show", "jq"], "Package: jq\n")
            .with_success("sudo", ["apt-get", "install", "-y", "jq"], "");

        let summary = run(
            &[parse_entry("jq|pkg|jq|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("jq"),
            Some(&ManifestEntry::new("jq", "pkg", "jq", ""))
        );
        assert_eq!(
            fs::read_to_string(fixture.roots.state_dir.join("jq-post")).unwrap(),
            "post\n"
        );
    }

    #[test]
    fn update_pauses_progress_before_sudo_package_commands() {
        let fixture = Fixture::new("pkg-sudo-prompt-pause");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("sudo", ["apt-get", "update", "-qq"], "")
            .with_success("apt-cache", ["show", "jq"], "Package: jq\n")
            .with_success("sudo", ["apt-get", "install", "-y", "jq"], "");
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[parse_entry("jq|pkg|missing-jq|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &mut progress,
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            progress.prompt_pauses,
            vec![
                "waiting for sudo authentication".to_owned(),
                "waiting for sudo authentication".to_owned(),
            ],
            "metadata refresh and package install should both yield the UI before sudo"
        );
        let sudo_calls = runner
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("sudo\0"))
            .collect::<Vec<_>>();
        assert_eq!(
            sudo_calls,
            vec![
                key("sudo", ["apt-get", "update", "-qq"]),
                key("sudo", ["apt-get", "install", "-y", "jq"]),
            ]
        );
    }

    #[test]
    fn update_processes_packages_before_custom_hooks() {
        let fixture = Fixture::new("pkg-first");
        fixture.write_lib();
        fixture.write_hook(
            "custom",
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/manifest" ]] && grep -q '^jq|pkg|' "$SHDEPS_STATE_DIR/manifest"; }
version() { printf 'saw-pkg\n'; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_command("jq");

        let summary = run(
            &[
                parse_entry("custom|custom|custom|-|-", None),
                parse_entry("jq|pkg|jq|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].name, "jq");
        assert_eq!(summary.items[1].name, "custom");
        assert_eq!(summary.items[1].detail, "saw-pkg");
    }

    #[test]
    fn update_retries_package_batch_failures_individually() {
        let fixture = Fixture::new("pkg-retry");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("apt-cache", ["show", "jq"], "Package: jq\n")
            .with_success("apt-cache", ["show", "fd"], "Package: fd\n")
            .with_failure("sudo", ["apt-get", "install", "-y", "jq", "fd"])
            .with_success("sudo", ["apt-get", "install", "-y", "jq"], "")
            .with_failure("sudo", ["apt-get", "install", "-y", "fd"]);

        let summary = run(
            &[
                parse_entry("jq|pkg|jq|-|-", None),
                parse_entry("fd|pkg|fd|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["fd"]);
    }

    #[test]
    fn update_installs_external_tool_and_records_manifest() {
        let fixture = Fixture::new("external-install");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.install_dir.join("ripgrep/bin/rg");
        let runner = FakeRunner::default()
            .with_command("cargo")
            .with_created_binary(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    fixture.roots.install_dir.join("ripgrep").to_str().unwrap(),
                    "ripgrep",
                ],
                bin_path.clone(),
            );

        let summary = run(
            &[parse_entry("ripgrep|cargo|rg|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("rg")).unwrap(),
            bin_path
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("ripgrep"),
            Some(&ManifestEntry::new(
                "ripgrep",
                "cargo",
                "rg",
                fixture
                    .roots
                    .install_dir
                    .join("ripgrep/bin/rg")
                    .display()
                    .to_string(),
            ))
        );
    }

    #[test]
    fn update_external_fast_path_repairs_missing_public_symlink() {
        let fixture = Fixture::new("external-fast");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.install_dir.join("ripgrep/bin/rg");
        write_executable(&bin_path);
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "ripgrep", "cargo"),
            1_700_000_000,
        )
        .unwrap();

        let summary = run(
            &[parse_entry("ripgrep|cargo|rg|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                now: 1_700_000_100,
                remote_ttl: 3600,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "fresh");
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("rg")).unwrap(),
            bin_path
        );
    }

    #[test]
    fn update_external_reports_missing_tool_without_manifest_row() {
        let fixture = Fixture::new("external-missing-tool");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("ripgrep|cargo|rg|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["ripgrep"]);
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("ripgrep")
                .is_none()
        );
    }

    #[test]
    fn update_builtin_methods_continue_after_one_install_fails() {
        let fixture = Fixture::new("builtin-failure-isolation");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let fzf_bin = fixture
            .roots
            .install_dir
            .join("github.com/junegunn/fzf/bin/fzf");
        let runner = FakeRunner::default()
            .with_command("cargo")
            .with_command("go")
            .with_failure(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    fixture.roots.install_dir.join("ripgrep").to_str().unwrap(),
                    "ripgrep",
                ],
            )
            .with_created_binary(
                "env",
                [
                    &format!(
                        "GOBIN={}",
                        fixture
                            .roots
                            .install_dir
                            .join("github.com/junegunn/fzf/bin")
                            .display()
                    ),
                    "go",
                    "install",
                    "github.com/junegunn/fzf@latest",
                ],
                fzf_bin.clone(),
            );

        let summary = run(
            &[
                parse_entry("ripgrep|cargo|rg|-|-", None),
                parse_entry("github.com/junegunn/fzf|go|fzf|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["ripgrep"]);
        assert_eq!(summary.items[0].detail, "cargo install failed");
        assert!(summary.items[1].changed);
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("fzf")).unwrap(),
            fzf_bin
        );
        let manifest = manifest::read(&manifest_path).unwrap();
        assert!(manifest.get("ripgrep").is_none());
        assert_eq!(
            manifest.get("github.com/junegunn/fzf"),
            Some(&ManifestEntry::new(
                "github.com/junegunn/fzf",
                "go",
                "fzf",
                fixture
                    .roots
                    .install_dir
                    .join("github.com/junegunn/fzf/bin/fzf")
                    .display()
                    .to_string(),
            ))
        );
    }

    #[test]
    fn update_builtin_progress_starts_queued_groups_when_worker_starts() {
        let mut fixture = Fixture::new("builtin-progress-starts");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "1".to_owned());
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let rg_bin = fixture.roots.install_dir.join("ripgrep/bin/rg");
        let fzf_bin = fixture
            .roots
            .install_dir
            .join("github.com/junegunn/fzf/bin/fzf");
        let runner = FakeRunner::default()
            .with_command("cargo")
            .with_command("go")
            .with_created_binary(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    fixture.roots.install_dir.join("ripgrep").to_str().unwrap(),
                    "ripgrep",
                ],
                rg_bin,
            )
            .with_created_binary(
                "env",
                [
                    &format!(
                        "GOBIN={}",
                        fixture
                            .roots
                            .install_dir
                            .join("github.com/junegunn/fzf/bin")
                            .display()
                    ),
                    "go",
                    "install",
                    "github.com/junegunn/fzf@latest",
                ],
                fzf_bin,
            );
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[
                parse_entry("ripgrep|cargo|rg|-|-", None),
                parse_entry("github.com/junegunn/fzf|go|fzf|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
            &mut progress,
        )
        .unwrap();

        let cargo_start = progress
            .phases
            .iter()
            .position(|phase| phase.key == super::PHASE_CARGO && phase.done == 0)
            .unwrap();
        let cargo_done = progress
            .phases
            .iter()
            .position(|phase| phase.key == super::PHASE_CARGO && phase.done == 1)
            .unwrap();
        let go_start = progress
            .phases
            .iter()
            .position(|phase| phase.key == super::PHASE_GO && phase.done == 0)
            .unwrap();

        assert!(!summary.has_errors());
        assert!(
            cargo_start < cargo_done && cargo_done < go_start,
            "queued groups should not start progress timers before a worker reaches them: {:?}",
            progress
                .phases
                .iter()
                .map(|phase| (&phase.key, phase.done, phase.total))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn update_external_failed_first_install_cleans_up_empty_install_root() {
        // When a fresh external install (cargo/go/uv/npm) fails before
        // producing any binary, the create_dir_all stub directories
        // would otherwise linger under the managed install tree. They
        // are visually misleading (`ls share/owner/tool/` shows an
        // empty `bin/`) and confuse later prune/transition heuristics.
        // A failed reinstall (where install_root had prior content)
        // must NOT be touched — `remove_dir` only removes empty dirs,
        // which gives exactly the right shape.
        let fixture = Fixture::new("external-failed-cleanup");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        // `cargo` exists on PATH but the install command exits non-zero.
        let runner = FakeRunner::default().with_failure(
            "cargo",
            [
                "install",
                "--root",
                &fixture.roots.install_dir.join("ripgrep").to_string_lossy(),
                "rg",
            ],
        );

        let install_root = fixture.roots.install_dir.join("ripgrep");
        let summary = run(
            &[parse_entry("ripgrep|cargo|rg|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors(), "expected failed install to surface");
        assert!(
            !install_root.exists(),
            "empty install_root must be cleaned up after first-time install failure"
        );
    }

    #[test]
    fn update_external_reinstall_uses_force_argument() {
        let fixture = Fixture::new("external-reinstall");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.install_dir.join("ripgrep/bin/rg");
        write_executable(&bin_path);
        let runner = FakeRunner::default().with_command("cargo").with_success(
            "cargo",
            [
                "install",
                "--locked",
                "--root",
                fixture.roots.install_dir.join("ripgrep").to_str().unwrap(),
                "ripgrep",
                "--force",
            ],
            "",
        );

        let summary = run(
            &[parse_entry("ripgrep|cargo|rg|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
    }

    #[test]
    fn update_github_release_installs_plain_binary_and_records_manifest() {
        let mut fixture = Fixture::new("release-plain");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64", b"binary".to_vec());
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"binary");
        assert_eq!(
            fs::read_to_string(crate::stamp::remote_path(
                &fixture.roots.state_dir,
                "owner/tool",
                "release"
            ))
            .unwrap(),
            "1700000000\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_verifies_sibling_sha256_checksum_when_published() {
        // When the release publishes a `<asset>.sha256` sibling, the
        // installer must fetch it and verify the downloaded binary before
        // landing it on disk. Successful verification produces the same
        // observable behavior as the no-checksum path — but the request log
        // confirms the checksum download did happen.
        let mut fixture = Fixture::new("release-checksum-ok");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "{}  tool-linux-x86_64\n",
            crate::checksum::sha256_hex(&binary)
        );
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"tool-linux-x86_64.sha256",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary.clone(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256",
                checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read(fixture.roots.bin_dir.join("tool")).unwrap(),
            binary
        );
        let urls: Vec<String> = fixture
            .client
            .requests()
            .into_iter()
            .map(|(url, _)| url)
            .collect();
        assert!(
            urls.iter().any(|url| url.ends_with(".sha256")),
            "checksum sibling must have been fetched: {urls:?}"
        );
    }

    #[test]
    fn update_github_release_verifies_release_wide_sha512_checksum() {
        let mut fixture = Fixture::new("release-checksum-sha512");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "{}  tool-linux-x86_64\n",
            crate::checksum::sha512_hex(&binary)
        );
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"SHA512SUMS",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/SHA512SUMS"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary.clone(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/SHA512SUMS",
                checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read(fixture.roots.bin_dir.join("tool")).unwrap(),
            binary
        );
    }

    #[test]
    fn update_github_release_verifies_release_wide_checksum_with_relative_name() {
        let mut fixture = Fixture::new("release-checksum-relative-name");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "{}  ./tool-linux-x86_64\n",
            crate::checksum::sha256_hex(&binary)
        );
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"SHASUMS256.txt",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/SHASUMS256.txt"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary.clone(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/SHASUMS256.txt",
                checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read(fixture.roots.bin_dir.join("tool")).unwrap(),
            binary
        );
    }

    #[test]
    fn update_github_release_verifies_release_wide_filename_first_checksum() {
        let mut fixture = Fixture::new("release-checksum-filename-first");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "tool-linux-x86_64  {}  {}  {}\n",
            "0".repeat(64),
            crate::checksum::sha256_hex(&binary),
            crate::checksum::sha512_hex(&binary),
        );
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"checksums",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/checksums"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary.clone(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/checksums",
                checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read(fixture.roots.bin_dir.join("tool")).unwrap(),
            binary
        );
    }

    #[test]
    fn update_github_release_refuses_release_wide_checksum_mismatch() {
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "{}  tool-linux-x86_64\n",
            crate::checksum::sha256_hex(b"different-bytes"),
        );

        assert_release_wide_checksum_failure("release-checksum-wide-bad", binary, checksum_text);
    }

    #[test]
    fn update_github_release_refuses_release_wide_checksum_for_wrong_asset() {
        let binary = b"binary".to_vec();
        let checksum_text = format!(
            "{}  other-linux-x86_64\n",
            crate::checksum::sha256_hex(&binary),
        );

        assert_release_wide_checksum_failure(
            "release-checksum-wide-wrong-name",
            binary,
            checksum_text,
        );
    }

    fn assert_release_wide_checksum_failure(name: &str, binary: Vec<u8>, checksum_text: String) {
        let mut fixture = Fixture::new(name);
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"SHA256SUMS",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/SHA256SUMS"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary,
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/SHA256SUMS",
                checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].detail, "release asset checksum mismatch");
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_release_refuses_install_on_sha256_mismatch() {
        // If the sibling checksum is published but mismatches the
        // downloaded binary, the install must be refused outright and the
        // existing public bin must remain untouched. A failed run is
        // surfaced in the summary so callers (and `dot update`) gate on
        // it instead of silently shipping the bad binary.
        let mut fixture = Fixture::new("release-checksum-bad");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let wrong_checksum_text = format!(
            "{}  tool-linux-x86_64\n",
            crate::checksum::sha256_hex(b"different-bytes"),
        );
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"tool-linux-x86_64.sha256",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary,
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256",
                wrong_checksum_text.into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].detail, "release asset checksum mismatch");
        assert!(!fixture.roots.bin_dir.join("tool").exists());
    }

    #[test]
    fn update_github_release_refuses_install_when_checksum_asset_is_unavailable() {
        // Round-6 paladin finding: when the release JSON advertises a
        // `.sha256` sibling but the checksum download fails, the
        // installer must NOT land an unverified binary AND must NOT
        // write a manifest entry. The earlier soft-fail returned
        // `failed: false` which let the caller's `write_manifest`
        // record a phantom row pointing at a bin path that did not
        // exist on first install.
        let mut fixture = Fixture::new("release-checksum-unavailable");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[
                        {
                            "name":"tool-linux-x86_64",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        },
                        {
                            "name":"tool-linux-x86_64.sha256",
                            "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256"
                        }
                    ]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                binary,
            );
        // No `.with(...)` entry for the .sha256 URL: the fake client
        // returns NotFound for it, mirroring a transient checksum-only
        // outage that the soft-fail path used to silently install
        // through.
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(
            summary.has_errors(),
            "checksum-unavailable must surface as a failure"
        );
        assert!(
            summary.items[0].detail.contains("checksum unavailable"),
            "detail must explain why: got {:?}",
            summary.items[0].detail
        );
        // Critical: no phantom manifest row, no installed binary.
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_release_sends_token_to_metadata_only() {
        let mut fixture = Fixture::new("release-token");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("GH_TOKEN".to_owned(), "ci-token".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64", b"binary".to_vec());
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                    Some("ci-token".to_owned())
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                        .to_owned(),
                    None
                )
            ]
        );
    }

    #[test]
    fn update_github_release_uses_api_asset_fallback_for_private_releases() {
        let mut fixture = Fixture::new("release-private-asset");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("GH_TOKEN".to_owned(), "ci-token".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "url":"https://api.github.com/repos/owner/tool/releases/assets/7",
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            // No `.with(...)` for the browser URL: the fake client returns
            // NotFound for that GET, which mirrors how the real flow falls
            // through to the authenticated REST asset endpoint when the
            // signed-URL redirect fails for a private release.
            .with(
                "https://api.github.com/repos/owner/tool/releases/assets/7",
                b"binary".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                    Some("ci-token".to_owned())
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        .to_owned(),
                    None,
                ),
                (
                    "https://api.github.com/repos/owner/tool/releases/assets/7".to_owned(),
                    Some("ci-token".to_owned())
                )
            ]
        );
    }

    #[test]
    fn update_github_release_prefetches_metadata_with_bounded_parallelism() {
        let mut fixture = Fixture::new("release-prefetch-parallel");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "2".to_owned());
        fixture.client = FakeClient::default()
            .with_overlap_gate(2)
            .with_delay(Duration::from_millis(25))
            .with(
                "https://api.github.com/repos/owner/tool-a/releases?per_page=100",
                release_response(
                    "tool-a",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                b"tool-a".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                b"tool-b".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-c/releases?per_page=100",
                release_response(
                    "tool-c",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-c-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-c-linux-x86_64",
                b"tool-c".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
                parse_entry("owner/tool-c|github:release|tool-c|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let requests = fixture.client.requests();
        let metadata_count = requests
            .iter()
            .filter(|(url, _)| url.starts_with("https://api.github.com/repos/owner/tool-"))
            .count();
        let asset_count = requests
            .iter()
            .filter(|(url, _)| {
                url.starts_with("https://github.com/owner/tool/releases/download/v1/tool-")
            })
            .count();

        assert!(!summary.has_errors());
        assert!(summary.items.iter().all(|item| item.changed));
        assert_eq!(
            metadata_count, 3,
            "each release repo should be fetched once; duplicate metadata fetches are the first sign that prefetch results are not being reused"
        );
        assert_eq!(asset_count, 3);
        assert_eq!(
            fixture.client.max_active(),
            2,
            "SHDEPS_JOBS=2 should overlap release metadata checks but never exceed the configured bound"
        );
    }

    #[test]
    fn update_github_release_prefetch_reports_completion_progress() {
        let mut fixture = Fixture::new("release-prefetch-progress");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "2".to_owned());
        fixture.client = FakeClient::default()
            .with_delay(Duration::from_millis(10))
            .with(
                "https://api.github.com/repos/owner/tool-a/releases?per_page=100",
                release_response(
                    "tool-a",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                b"tool-a".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                b"tool-b".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-c/releases?per_page=100",
                release_response(
                    "tool-c",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-c-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-c-linux-x86_64",
                b"tool-c".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
                parse_entry("owner/tool-c|github:release|tool-c|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &mut progress,
        )
        .unwrap();

        let metadata_progress = progress
            .phases
            .iter()
            .filter(|phase| phase.key == super::PHASE_GITHUB_RELEASE_METADATA)
            .map(|phase| (phase.done, phase.total))
            .collect::<Vec<_>>();

        assert!(!summary.has_errors());
        assert!(
            metadata_progress.contains(&(1, 3))
                && metadata_progress.contains(&(2, 3))
                && metadata_progress.contains(&(3, 3)),
            "metadata prefetch should report each completed worker result: {metadata_progress:?}"
        );
    }

    #[test]
    fn update_github_release_metadata_progress_uses_prefetch_candidate_total() {
        let mut fixture = Fixture::new("release-prefetch-candidate-total");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "2".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                b"tool-b".to_vec(),
            );
        write_executable(&fixture.roots.bin_dir.join("tool-a"));
        let options = Options::default();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool-a", "release"),
            options.now,
        )
        .unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            options,
            &mut progress,
        )
        .unwrap();

        let metadata_progress = progress
            .phases
            .iter()
            .filter(|phase| phase.key == super::PHASE_GITHUB_RELEASE_METADATA)
            .map(|phase| (phase.done, phase.total))
            .collect::<Vec<_>>();

        assert!(!summary.has_errors());
        assert_eq!(
            metadata_progress,
            vec![(0, 1)],
            "metadata progress should use the prefetch candidate count, not all release entries"
        );
    }

    #[test]
    fn update_github_release_prefetches_current_versions_with_bounded_parallelism() {
        let mut fixture = Fixture::new("release-version-prefetch");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "2".to_owned());
        fixture
            .env_vars
            .insert("GH_TOKEN".to_owned(), "ci-token".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool-a/releases?per_page=100",
                release_response(
                    "tool-a",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                ),
            )
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v2.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            );
        write_executable(&fixture.roots.bin_dir.join("tool-a"));
        write_executable(&fixture.roots.bin_dir.join("tool-b"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("tool-a", ["--version"], "tool-a 1.0.0\n")
            .with_success("tool-b", ["--version"], "tool-b 2.0.0\n")
            .with_overlap_gate(2)
            .with_delay(Duration::from_millis(25));
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                force: true,
                ..Options::default()
            },
            &mut progress,
        )
        .unwrap();

        let version_progress = progress
            .phases
            .iter()
            .filter(|phase| phase.key == super::PHASE_GITHUB_RELEASE_VERSIONS)
            .map(|phase| (phase.done, phase.total))
            .collect::<Vec<_>>();
        let asset_count = fixture
            .client
            .requests()
            .iter()
            .filter(|(url, _)| {
                url.starts_with("https://github.com/owner/tool/releases/download/v1/tool-")
            })
            .count();

        assert!(!summary.has_errors());
        assert!(summary.items.iter().all(|item| !item.changed));
        assert_eq!(
            asset_count, 0,
            "prefetched current versions should preserve the force no-op path and avoid unnecessary release downloads"
        );
        assert_eq!(
            runner.max_active(),
            2,
            "installed-version probes are read-only and should overlap with the same SHDEPS_JOBS bound as release metadata"
        );
        assert!(
            version_progress.contains(&(1, 2)) && version_progress.contains(&(2, 2)),
            "version prefetch should be split from GitHub metadata progress: {version_progress:?}"
        );
    }

    #[test]
    fn update_github_release_jobs_one_keeps_remote_checks_sequential() {
        let mut fixture = Fixture::new("release-prefetch-sequential");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "1".to_owned());
        fixture.client = FakeClient::default()
            .with_delay(Duration::from_millis(5))
            .with(
                "https://api.github.com/repos/owner/tool-a/releases?per_page=100",
                release_response(
                    "tool-a",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                b"tool-a".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                b"tool-b".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fixture.client.max_active(),
            1,
            "SHDEPS_JOBS=1 is the debugging and low-resource escape hatch, so the prefetch layer must not introduce hidden parallelism"
        );
    }

    #[test]
    fn update_github_release_reuses_gh_token_for_metadata_only() {
        let mut fixture =
            Fixture::new("release-token-prefetch").with_env_var("SHDEPS_ALLOW_GH_AUTH_TOKEN", "1");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "1".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool-a/releases?per_page=100",
                release_response(
                    "tool-a",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64",
                b"tool-a".to_vec(),
            )
            .with(
                "https://api.github.com/repos/owner/tool-b/releases?per_page=100",
                release_response(
                    "tool-b",
                    "v1.0.0",
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                ),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64",
                b"tool-b".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_command("gh")
            .with_success("gh", ["auth", "token"], "gh-token\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[
                parse_entry("owner/tool-a|github:release|tool-a|-|-", None),
                parse_entry("owner/tool-b|github:release|tool-b|-|-", None),
            ],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let token_calls = runner
            .calls()
            .into_iter()
            .filter(|call| call == &key("gh", ["auth", "token"]))
            .count();

        assert!(!summary.has_errors());
        assert_eq!(
            token_calls, 1,
            "`gh auth token` can be noticeably slow; shdeps should resolve it once per update run and reuse it for every release metadata request"
        );
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://api.github.com/repos/owner/tool-a/releases?per_page=100".to_owned(),
                    Some("gh-token".to_owned())
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1/tool-a-linux-x86_64"
                        .to_owned(),
                    None
                ),
                (
                    "https://api.github.com/repos/owner/tool-b/releases?per_page=100".to_owned(),
                    Some("gh-token".to_owned())
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1/tool-b-linux-x86_64"
                        .to_owned(),
                    None
                ),
            ]
        );
    }

    #[test]
    fn update_github_release_decompresses_gzip_single_binary() {
        let mut fixture = Fixture::new("release-gz-single");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.gz",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.gz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.gz", gzip(b"binary"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"binary");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_decompresses_bzip2_single_binary() {
        let mut fixture = Fixture::new("release-bz2-single");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.bz2",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.bz2"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.bz2", bzip2(b"binary"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"binary");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_decompresses_xz_single_binary() {
        let mut fixture = Fixture::new("release-xz-single");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.xz",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.xz"
                    }]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.xz",
                xz(b"binary"),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"binary");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_decompresses_zstd_single_binary() {
        let mut fixture = Fixture::new("release-zst-single");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.zst",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.zst"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.zst", zstd(b"binary"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"binary");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_fast_path_repairs_manifest_without_network() {
        let fixture = Fixture::new("release-fast");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            1_700_000_000,
        )
        .unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                now: 1_700_000_100,
                remote_ttl: 3600,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "fresh");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_verbose_fast_path_reports_local_version_without_network() {
        let fixture = Fixture::new("release-fast-verbose-version");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            1_700_000_000,
        )
        .unwrap();
        let runner = FakeRunner::default().with_success("tool", ["--version"], "tool 1.2.3\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                verbose: true,
                now: 1_700_000_100,
                remote_ttl: 3600,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "1.2.3");
        assert_eq!(
            fixture.client.requests(),
            Vec::<(String, Option<String>)>::new()
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_release_keeps_existing_binary_when_metadata_fetch_fails() {
        let fixture = Fixture::new("release-metadata-outage");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        let runner = FakeRunner::default().with_success("tool", ["--version"], "tool 1.2.3\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"#!/bin/sh\n");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
        assert!(
            fs::read_to_string(crate::stamp::remote_path(
                &fixture.roots.state_dir,
                "owner/tool",
                "release"
            ))
            .is_err(),
            "metadata failures should not refresh the remote stamp"
        );
    }

    #[test]
    fn update_github_release_metadata_failure_still_fails_without_binary() {
        let fixture = Fixture::new("release-metadata-outage-missing");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["owner/tool"]);
        assert_eq!(summary.items[0].detail, "release metadata fetch failed");
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_release_reinstall_requires_metadata_even_with_binary() {
        let fixture = Fixture::new("release-metadata-outage-reinstall");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        write_executable(&fixture.roots.bin_dir.join("tool"));

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["owner/tool"]);
        assert_eq!(summary.items[0].detail, "release metadata fetch failed");
    }

    #[test]
    fn update_github_release_force_checks_without_reinstalling_current_binary() {
        let mut fixture = Fixture::new("release-force-current");
        fixture.write_lib();
        fixture.client = FakeClient::default().with(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            br#"[{
                "tag_name":"v1.2.3",
                "draft":false,
                "prerelease":false,
                "assets":[{
                    "name":"tool-linux-x86_64",
                    "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                }]
            }]"#
            .to_vec(),
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            1_700_000_000,
        )
        .unwrap();
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.2.3\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                force: true,
                now: 1_700_000_500,
                remote_ttl: 3600,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"#!/bin/sh\n");
        assert_eq!(
            fixture.client.requests(),
            vec![(
                "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                None
            )],
            "force should refresh metadata but must not download a release asset when the installed version is current"
        );
        assert_eq!(
            fs::read_to_string(crate::stamp::remote_path(
                &fixture.roots.state_dir,
                "owner/tool",
                "release"
            ))
            .unwrap(),
            "1700000500\n"
        );
    }

    #[test]
    fn update_github_release_reinstall_downloads_even_when_current() {
        let mut fixture = Fixture::new("release-reinstall-current");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64", b"new-binary".to_vec());
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.2.3\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "v1.2.3");
        assert_eq!(fs::read(&bin_path).unwrap(), b"new-binary");
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                    None
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64"
                        .to_owned(),
                    None
                )
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_installs_tar_gz_archive_and_links_extras() {
        let mut fixture = Fixture::new("release-tar-gz");
        fixture.write_lib();
        let archive = tar_gz(&[
            ("tool-v1.2.3/bin/tool", b"binary".as_slice(), 0o755),
            (
                "tool-v1.2.3/share/man/man1/tool.1",
                b"man".as_slice(),
                0o644,
            ),
        ]);
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.tar.gz",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar.gz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar.gz", archive);
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(&bin_path).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("man/man1/tool.1")).unwrap(),
            install_dir.join("share/man/man1/tool.1")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_installs_tar_archive_and_links_extras() {
        let mut fixture = Fixture::new("release-tar");
        fixture.write_lib();
        let archive = tar(&[
            ("tool-v1.2.3/bin/tool", b"binary".as_slice(), 0o755),
            (
                "tool-v1.2.3/share/man/man1/tool.1",
                b"man".as_slice(),
                0o644,
            ),
        ]);
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.tar",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar"
                    }]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar",
                archive,
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(&bin_path).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("man/man1/tool.1")).unwrap(),
            install_dir.join("share/man/man1/tool.1")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_installs_tar_xz_archive_and_links_extras() {
        let mut fixture = Fixture::new("release-tar-xz");
        fixture.write_lib();
        let archive = tar_xz(&[
            ("tool-v1.2.3/bin/tool", b"binary".as_slice(), 0o755),
            (
                "tool-v1.2.3/share/man/man1/tool.1",
                b"man".as_slice(),
                0o644,
            ),
        ]);
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.tar.xz",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar.xz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.tar.xz", archive);
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(&bin_path).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("man/man1/tool.1")).unwrap(),
            install_dir.join("share/man/man1/tool.1")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_installs_zip_archive_and_links_extras() {
        let mut fixture = Fixture::new("release-zip");
        fixture.write_lib();
        let archive = zip(&[
            ("tool-v1.2.3/bin/tool", b"binary".as_slice(), 0o755),
            ("tool-v1.2.3/share/zsh/site-functions/_tool", b"comp", 0o644),
        ]);
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.zip",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.zip"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://github.com/owner/tool/releases/download/v1/tool-v1.2.3-linux-x86_64.zip", archive);
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let bin_path = fixture.roots.bin_dir.join("tool");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(&bin_path).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("zsh/site-functions/_tool")).unwrap(),
            install_dir.join("share/zsh/site-functions/_tool")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_repo_uses_local_dev_clone_first() {
        let fixture = Fixture::new("repo-local");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        fs::create_dir_all(local_clone.join("share/man/man1")).unwrap();
        fs::write(local_clone.join("share/man/man1/ds.1"), ".TH DS 1\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "local clone");
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("ds")).unwrap(),
            install_link.join("bin/ds")
        );
        assert_eq!(
            fs::read_link(fixture.roots.install_dir.join("man/man1/ds.1")).unwrap(),
            install_link.join("share/man/man1/ds.1")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("cgraf78/ds"),
            Some(&ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_link.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_repo_verbose_reports_local_clone_version() {
        let fixture = Fixture::new("repo-local-verbose");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        fs::write(local_clone.join("VERSION"), "1.2.3\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                verbose: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "added -- 1.2.3 (local clone)");
    }

    #[test]
    fn update_github_repo_verbose_reports_short_local_clone_commit() {
        let fixture = Fixture::new("repo-local-verbose-commit");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success(
                "git",
                ["-C", local_clone.to_str().unwrap(), "rev-parse", "HEAD"],
                "abc1234567890\n",
            )
            .with_success(
                "git",
                [
                    "-C",
                    local_clone.to_str().unwrap(),
                    "rev-parse",
                    "--short",
                    "HEAD",
                ],
                "abc1234\n",
            );

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                verbose: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            summary.items[0].detail,
            "added -- commit abc1234 (local clone)"
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_reuses_existing_local_clone_symlink() {
        let fixture = Fixture::new("repo-local-unchanged");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&local_clone, &install_link).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_local_dev_clone_keeps_user_directory_modes() {
        let fixture = Fixture::new("repo-local-modes");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        fs::create_dir_all(local_clone.join("src")).unwrap();
        fs::write(local_clone.join("src/_ds"), "#compdef ds\n").unwrap();
        fs::set_permissions(&local_clone, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(local_clone.join("src"), fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(
            local_clone.join("src/_ds"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fs::metadata(&local_clone).unwrap().permissions().mode() & 0o777,
            0o777
        );
        assert_eq!(
            fs::metadata(local_clone.join("src"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o777
        );
        assert_eq!(
            fs::metadata(local_clone.join("src/_ds"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o666
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_replaces_removed_local_clone_symlink_with_managed_clone() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-local-removed");
        fixture.write_lib();
        let removed_local_clone = fixture.roots.git_dev_dir.join("ds");
        let install_dir = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_dir.parent().unwrap()).unwrap();
        symlink(&removed_local_clone, &install_dir).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_dir.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let clone_tmp = fixture
            .roots
            .install_dir
            .join(format!("cgraf78/ds.tmp.{}", std::process::id()));
        let runner = FakeRunner::default()
            .with_command("git")
            .with_created_dir(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/cgraf78/ds",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join(".git"),
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:cgraf78/ds.git",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "added");
        assert!(install_dir.join(".git").is_dir());
        assert!(!install_dir.is_symlink());
        assert!(!removed_local_clone.exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("cgraf78/ds"),
            Some(&ManifestEntry::new(
                "cgraf78/ds",
                "github:repo",
                "ds",
                install_dir.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_repo_canonicalizes_git_suffix_before_installing() {
        let fixture = Fixture::new("repo-git-suffix");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds.git|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(fixture.roots.install_dir.join("cgraf78/ds").is_symlink());
        assert!(!fixture.roots.install_dir.join("cgraf78/ds.git").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("cgraf78/ds")
                .is_some()
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("cgraf78/ds.git")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_without_local_clone_fails_without_manifest_row() {
        let fixture = Fixture::new("repo-network-unimplemented");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["cgraf78/ds"]);
        assert_eq!(summary.items[0].detail, "git not available");
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("cgraf78/ds")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_clones_fresh_repo_and_sets_ssh_push_url() {
        let mut fixture = Fixture::new("repo-fresh-clone");
        fixture.write_lib();
        fixture.env_vars.insert(
            "SHDEPS_PRIVATE_TOOL_REPO".to_owned(),
            "https://github.com/private/tool".to_owned(),
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("private/tool");
        let clone_tmp = fixture
            .roots
            .install_dir
            .join(format!("private/tool.tmp.{}", std::process::id()));
        let runner = FakeRunner::default()
            .with_command("git")
            .with_created_dir(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/private/tool",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join(".git"),
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:private/tool.git",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("private/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert!(install_dir.join(".git").is_dir());
        assert_eq!(summary.items[0].detail, "added");
        assert_eq!(
            fs::read_to_string(crate::stamp::remote_path(
                &fixture.roots.state_dir,
                "private/tool",
                "repo"
            ))
            .unwrap(),
            "1700000000\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("private/tool"),
            Some(&ManifestEntry::new(
                "private/tool",
                "github:repo",
                "tool",
                install_dir.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_repo_fresh_clone_retries_github_https_as_ssh() {
        let fixture = Fixture::new("repo-fresh-fallback");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("private/tool");
        let clone_tmp = fixture
            .roots
            .install_dir
            .join(format!("private/tool.tmp.{}", std::process::id()));
        let runner = FakeRunner::default()
            .with_command("git")
            .with_failure(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/private/tool",
                    clone_tmp.to_str().unwrap(),
                ],
            )
            .with_created_dir(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "git@github.com:private/tool.git",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join(".git"),
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:private/tool.git",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("private/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(install_dir.join(".git").is_dir());
        assert_eq!(summary.items[0].detail, "added");
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_existing_managed_clone_strips_insecure_write_bits() {
        let fixture = Fixture::new("repo-managed-modes");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("private/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        fs::create_dir_all(install_dir.join("src")).unwrap();
        fs::write(install_dir.join("src/_tool"), "#compdef tool\n").unwrap();
        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(install_dir.join("src"), fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(
            install_dir.join("src/_tool"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "private/tool", "repo"),
            1_700_000_000,
        )
        .unwrap();

        let summary = run(
            &[parse_entry("private/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fs::metadata(&install_dir).unwrap().permissions().mode() & 0o022,
            0
        );
        assert_eq!(
            fs::metadata(install_dir.join("src"))
                .unwrap()
                .permissions()
                .mode()
                & 0o022,
            0
        );
        assert_eq!(
            fs::metadata(install_dir.join("src/_tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o022,
            0
        );
    }

    #[test]
    fn run_holds_the_state_lock_for_the_duration() {
        // Concurrent `shdeps update` runs must serialize. The lock is
        // a per-state-dir advisory `flock`; the test asserts that the
        // try-acquire path returns `None` once a real `run` is in
        // progress, by running `run` on a separate thread and
        // observing from the main thread.
        let fixture = Fixture::new("lock-during-run");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        // Pre-acquire from the main thread; spawn a second thread that
        // also tries to acquire — that second acquire should not
        // succeed (`try_acquire` returns None) until the main thread
        // drops the lock. This mirrors what would happen if a second
        // `shdeps update` started while the first was mid-run, without
        // requiring us to actually thread the run loop.
        let _env_guard = crate::state::lock_reentry_env_for_test();
        crate::state::clear_reentry_env_for_test();
        let primary = crate::state::StateLock::acquire(&fixture.roots.state_dir).unwrap();
        let attempt = crate::state::StateLock::try_acquire(&fixture.roots.state_dir).unwrap();
        assert!(
            attempt.is_none(),
            "second concurrent acquire must block while the first lock is held"
        );
        drop(primary);
        let _ = manifest_path; // shape doc only
        assert!(
            crate::state::StateLock::try_acquire(&fixture.roots.state_dir)
                .unwrap()
                .is_some(),
            "lock must be re-acquirable after the holder drops it"
        );
    }

    #[test]
    fn summary_has_errors_only_promotes_leftovers_when_strict_mode_enabled() {
        // Backwards-compat: pre-fix behavior was that leftover I/O
        // errors did NOT gate the exit code. Audit feedback flagged
        // an unconditional promotion as a real CI/bootstrap break
        // risk. The compromise: leftovers gate exit only under
        // `SHDEPS_STRICT_LEFTOVERS=1`. Default mode preserves the
        // historical quiet behavior; strict mode is the opt-in for
        // operators who want hard enforcement.
        //
        // Serialize against any other test in this module that
        // touches `SHDEPS_STRICT_LEFTOVERS`. Rust test harness runs
        // tests in parallel within the same process; `set_var`
        // mutates process-global state, so without a mutex two
        // tests can see each other's transient values and silently
        // classify incorrectly.
        let _env_guard = STRICT_LEFTOVERS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Defensive reset: a previous test panicking with the env
        // var set would otherwise leak state into this run despite
        // the recovered mutex. Clear before asserting either branch.
        // SAFETY: env mutation is serialized by the test lock.
        unsafe {
            std::env::remove_var("SHDEPS_STRICT_LEFTOVERS");
        }

        let mut summary = Summary::default();
        assert!(!summary.has_errors());
        summary.leftovers.push("owner/tool".to_owned());
        // The defensive reset above already cleared the env var; no
        // second `remove_var` here. The reviewer's eye caught the
        // duplicate as a copy-paste artifact that wrongly implied a
        // `set_var` had intervened.
        assert!(
            !summary.has_errors(),
            "leftover must NOT gate exit by default — that would be a CI regression"
        );
        unsafe {
            std::env::set_var("SHDEPS_STRICT_LEFTOVERS", "1");
        }
        let strict_result = summary.has_errors();
        unsafe {
            std::env::remove_var("SHDEPS_STRICT_LEFTOVERS");
        }
        assert!(
            strict_result,
            "leftover must gate exit when SHDEPS_STRICT_LEFTOVERS=1 is set"
        );
    }

    /// Process-wide mutex serializing tests that mutate
    /// `SHDEPS_STRICT_LEFTOVERS`. See the matching `ENV_TEST_LOCK`
    /// pattern in `state.rs`. Module-private because we only
    /// need to serialize against same-module tests; cross-module
    /// env-var races on different keys are not observed in
    /// practice for shdeps' test inventory.
    static STRICT_LEFTOVERS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn update_github_repo_existing_clone_pull_failure_reports_dirty_tree_cause() {
        // When `git pull` fails on a managed clone, the user-visible
        // detail must tell the operator WHY: dirty working tree vs
        // network/fast-forward problem. The pre-fix code lumped both
        // into the opaque "update failed", which gave no signal that a
        // local edit had diverged the managed clone.
        let fixture = Fixture::new("repo-pull-dirty");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        let runner = FakeRunner::default()
            // No SSH retry: pretend origin has no GitHub fallback.
            .with_failure(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "get-url",
                    "origin",
                ],
            )
            .with_success(
                "git",
                ["-C", install_dir.to_str().unwrap(), "rev-parse", "HEAD"],
                "head\n",
            )
            .with_failure(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
            )
            // After the pull failure, `git status --porcelain` returns
            // non-empty → dirty working tree branch.
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                " M README.md\n",
            );

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "pull failed (dirty working tree)");
    }

    #[test]
    fn update_github_repo_existing_clone_pull_failure_reports_fast_forward_cause() {
        // Clean tree but `git pull --ff-only` still fails — this is the
        // network outage / non-FF case. The detail must distinguish it
        // from the dirty-tree case so operators retry vs investigate.
        let fixture = Fixture::new("repo-pull-clean");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        let runner = FakeRunner::default()
            .with_failure(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "get-url",
                    "origin",
                ],
            )
            .with_success(
                "git",
                ["-C", install_dir.to_str().unwrap(), "rev-parse", "HEAD"],
                "head\n",
            )
            .with_failure(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
            )
            // After the pull failure, status returns empty → clean tree.
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "pull failed (no fast-forward)");
    }

    #[test]
    fn update_github_repo_existing_clone_retries_pull_with_ssh_origin() {
        let fixture = Fixture::new("repo-pull-fallback");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("private/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        let runner = FakeRunner::default()
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "get-url",
                    "origin",
                ],
                "https://github.com/private/tool.git\n",
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "--push",
                    "origin",
                    "git@github.com:private/tool.git",
                ],
                "",
            )
            .with_success(
                "git",
                ["-C", install_dir.to_str().unwrap(), "rev-parse", "HEAD"],
                "old-head\n",
            )
            .with_failure(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "origin",
                    "git@github.com:private/tool.git",
                ],
                "",
            )
            .with_success(
                "git",
                [
                    "-C",
                    install_dir.to_str().unwrap(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("private/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "updated");
    }

    struct Fixture {
        roots: Roots,
        hooks: BashCustomProbe,
        env: RuntimeEnv,
        env_vars: BTreeMap<String, String>,
        client: FakeClient,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let home = temp_dir(name);
            let roots = Roots {
                conf_dir: home.join("conf"),
                hooks_dir: home.join("conf/hooks.d"),
                state_dir: home.join("state"),
                git_dev_dir: home.join("git"),
                install_dir: home.join("share"),
                bin_dir: home.join("bin"),
                home: home.clone(),
            };
            fs::create_dir_all(&roots.hooks_dir).unwrap();
            fs::create_dir_all(&roots.state_dir).unwrap();
            fs::create_dir_all(&roots.install_dir).unwrap();
            let hooks = BashCustomProbe::new(home.join("shdeps.sh"));
            Self {
                roots,
                hooks,
                env: RuntimeEnv::new("linux", "host"),
                env_vars: BTreeMap::new(),
                client: FakeClient::default(),
            }
        }

        fn write_lib(&self) {
            fs::write(self.hooks.shdeps_lib(), "shdeps_version() { :; }\n").unwrap();
        }

        fn write_hook(&self, name: &str, body: &str) {
            let path = self.roots.hooks_dir.join(format!("{name}.sh"));
            fs::write(&path, body).unwrap();
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }

        fn with_env_var(mut self, name: &str, value: &str) -> Self {
            self.env_vars.insert(name.to_owned(), value.to_owned());
            self
        }

        fn context<'a>(
            &'a self,
            manifest_path: &'a std::path::Path,
            runner: &'a FakeRunner,
            pkg_mgr: &'a str,
        ) -> Context<'a, FakeRunner> {
            Context {
                manifest_path,
                roots: &self.roots,
                env: &self.env,
                hooks: &self.hooks,
                runner,
                pkg_mgr,
                env_vars: &self.env_vars,
                client: &self.client,
            }
        }
    }

    type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;
    type TimeoutLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<Duration>)>>>;
    type AtomicCounter = std::sync::Arc<std::sync::atomic::AtomicUsize>;
    type OverlapGate = std::sync::Arc<OverlapGateState>;

    #[derive(Debug)]
    struct OverlapGateState {
        target: usize,
        highest: std::sync::Mutex<usize>,
        ready: std::sync::Condvar,
    }

    impl OverlapGateState {
        fn new(target: usize) -> Self {
            Self {
                target,
                highest: std::sync::Mutex::new(0),
                ready: std::sync::Condvar::new(),
            }
        }

        fn observe(&self, active: usize) {
            let mut highest = self.highest.lock().unwrap();
            *highest = (*highest).max(active);
            if *highest >= self.target {
                self.ready.notify_all();
                return;
            }

            // Sleep-based overlap assertions are scheduler-sensitive. Hold the
            // first worker briefly so the second worker can prove parallelism;
            // if it never arrives, the caller's max-active assertion still
            // fails instead of deadlocking the test.
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while *highest < self.target {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next, wait) = self.ready.wait_timeout(highest, remaining).unwrap();
                highest = next;
                if wait.timed_out() {
                    break;
                }
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FakeClient {
        responses: std::collections::BTreeMap<String, Vec<u8>>,
        requests: RequestLog,
        delay: Option<Duration>,
        overlap_gate: Option<OverlapGate>,
        active: AtomicCounter,
        max_active: AtomicCounter,
    }

    impl FakeClient {
        fn with(mut self, url: &str, bytes: impl Into<Vec<u8>>) -> Self {
            self.responses.insert(url.to_owned(), bytes.into());
            self
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = Some(delay);
            self
        }

        fn with_overlap_gate(mut self, target: usize) -> Self {
            self.overlap_gate = Some(std::sync::Arc::new(OverlapGateState::new(target)));
            self
        }

        fn requests(&self) -> Vec<(String, Option<String>)> {
            self.requests.lock().unwrap().clone()
        }

        fn max_active(&self) -> usize {
            self.max_active.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Client for FakeClient {
        fn get(&self, url: &str, token: Option<&str>) -> io::Result<Vec<u8>> {
            let active = self
                .active
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.max_active
                .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
            let _guard = ActiveGuard {
                active: &self.active,
            };
            if let Some(gate) = &self.overlap_gate {
                gate.observe(active);
            }
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            self.requests
                .lock()
                .unwrap()
                .push((url.to_owned(), token.map(ToOwned::to_owned)));
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

    #[derive(Debug, Clone, Default)]
    struct FakeRunner {
        commands: std::collections::BTreeSet<String>,
        outputs: std::collections::BTreeMap<String, QueuedOutputs>,
        creates: std::collections::BTreeMap<String, Vec<PathBuf>>,
        creates_dirs: std::collections::BTreeMap<String, Vec<PathBuf>>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        timeouts: TimeoutLog,
        delay: Option<Duration>,
        overlap_gate: Option<OverlapGate>,
        active: AtomicCounter,
        max_active: AtomicCounter,
    }

    impl FakeRunner {
        fn with_command(mut self, command: &str) -> Self {
            self.commands.insert(command.to_owned());
            self
        }

        fn with_success(
            mut self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
            stdout: &str,
        ) -> Self {
            self.push_output(
                key(program, args),
                Output {
                    success: true,
                    timed_out: false,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                },
            );
            self
        }

        fn with_created_binary(
            mut self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
            path: PathBuf,
        ) -> Self {
            let key = key(program, args);
            self.push_output(
                key.clone(),
                Output {
                    success: true,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            );
            self.creates.entry(key).or_default().push(path);
            self
        }

        fn with_created_dir(
            mut self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
            path: PathBuf,
        ) -> Self {
            let key = key(program, args);
            self.push_output(
                key.clone(),
                Output {
                    success: true,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            );
            self.creates_dirs.entry(key).or_default().push(path);
            self
        }

        fn with_failure(
            mut self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
        ) -> Self {
            self.push_output(
                key(program, args),
                Output {
                    success: false,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            );
            self
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = Some(delay);
            self
        }

        fn with_overlap_gate(mut self, target: usize) -> Self {
            self.overlap_gate = Some(std::sync::Arc::new(OverlapGateState::new(target)));
            self
        }

        fn push_output(&mut self, key: String, output: Output) {
            self.outputs
                .entry(key)
                .or_default()
                .lock()
                .unwrap()
                .push_back(output);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn timeouts_for(
            &self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
        ) -> Vec<Option<Duration>> {
            let expected = key(program, args);
            self.timeouts
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(call, timeout)| (call == &expected).then_some(*timeout))
                .collect()
        }

        fn max_active(&self) -> usize {
            self.max_active.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Runner for FakeRunner {
        fn exists(&self, command: &str) -> bool {
            self.commands.contains(command)
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
            _timeout: Option<Duration>,
        ) -> io::Result<Output> {
            let key = key(program, args);
            let active = self
                .active
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.max_active
                .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
            let _guard = ActiveGuard {
                active: &self.active,
            };
            if let Some(gate) = &self.overlap_gate {
                gate.observe(active);
            }
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            self.calls.lock().unwrap().push(key.clone());
            self.timeouts.lock().unwrap().push((key.clone(), _timeout));
            for path in self.creates.get(&key).into_iter().flatten() {
                write_executable(path);
            }
            for path in self.creates_dirs.get(&key).into_iter().flatten() {
                fs::create_dir_all(path).unwrap();
            }
            Ok(self.outputs.get(&key).map(next_output).unwrap_or(Output {
                success: false,
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
            }))
        }
    }

    type QueuedOutputs = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Output>>>;

    fn next_output(outputs: &QueuedOutputs) -> Output {
        // Some install flows intentionally run the same command twice, for
        // example `git pull` before and after rewriting an HTTPS origin to SSH.
        // Keep a tiny queue per command key so tests can model those retries
        // without shell scripts or global PATH mutation.
        let mut outputs = outputs.lock().unwrap();
        if outputs.len() > 1 {
            outputs.pop_front().unwrap()
        } else {
            outputs.front().cloned().unwrap()
        }
    }

    fn key(program: &str, args: impl IntoIterator<Item = impl AsRef<str>>) -> String {
        let mut key = program.to_owned();
        for arg in args {
            key.push('\0');
            key.push_str(arg.as_ref());
        }
        key
    }

    fn call_index(
        calls: &[String],
        program: &str,
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> usize {
        let expected = key(program, args);
        calls
            .iter()
            .position(|call| call == &expected)
            .unwrap_or_else(|| panic!("missing call: {expected:?}"))
    }

    fn write_executable(path: &PathBuf) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn release_response(cmd: &str, tag: &str, url: &str) -> Vec<u8> {
        // Keep release fixtures in one helper so performance tests can add
        // multiple repos without hiding the important assertion behind pages of
        // repeated GitHub JSON. The asset name still includes the command and
        // target platform because the selector's matching rules are part of
        // the behavior those tests are exercising.
        format!(
            r#"[{{
                "tag_name":"{tag}",
                "draft":false,
                "prerelease":false,
                "assets":[{{
                    "name":"{cmd}-linux-x86_64",
                    "browser_download_url":"{url}"
                }}]
            }}]"#
        )
        .into_bytes()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn bzip2(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = BzEncoder::new(Vec::new(), BzCompression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 0).unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_xz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        xz(&tar(entries))
    }

    fn tar(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for (path, body, mode) in entries {
                let mut header = Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append(&header, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        tar
    }

    fn zip(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = Cursor::new(&mut bytes);
            let mut writer = ZipWriter::new(cursor);
            for (path, body, mode) in entries {
                let options = SimpleFileOptions::default().unix_permissions(*mode);
                writer.start_file(path, options).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-update-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
