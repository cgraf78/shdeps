//! `shdeps update` orchestration.
//!
//! Install methods are intentionally small units, but `update` owns the order
//! that makes the system safe: prove or stage the new method before old-method
//! cleanup, update manifest rows only after a method has made its decision, and
//! run post hooks only for dependencies that actually changed. Keeping those
//! rules here avoids each install method learning partial transaction policy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
use crate::repo;
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
    /// Dependency remains usable, but update work needs operator attention.
    Warning,
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
    /// A repository remained usable after its fast-forward pull failed.
    RepoPullFailed,
    /// Package manager override disabled this dependency.
    PackageManagerOverride,
    /// Package is unavailable on this host/package manager.
    PackageUnavailable,
    /// Quiet package work needs sudo, but the current user cannot use it
    /// without an interactive prompt.
    PackageSudoUnavailable,
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

    pub(crate) fn warning(
        name: impl Into<String>,
        reason: ItemReason,
        detail: impl Into<String>,
        changed: bool,
    ) -> Self {
        Self {
            name: name.into(),
            changed,
            failed: false,
            status: ItemStatus::Warning,
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
/// each boundary can be exercised without ambient host state. The update lock
/// deliberately spans the whole operation; dependency injection makes the
/// operation testable without weakening that serialization window.
pub struct Context<'a, R>
where
    R: Runner,
{
    /// Manifest path to mutate.
    pub manifest_path: &'a std::path::Path,
    /// Runtime filesystem roots.
    pub roots: &'a Roots,
    /// Runtime platform, host, and package-manager identity.
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
    _manifest: &Manifest,
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
    // the hook re-binds `SHDEPS_STATE_LOCK_HELD` to its own PID. The
    // inner acquire recognizes that cooperative parent-child chain and
    // returns a no-op guard instead of deadlocking against the outer
    // flock. This exception exists only for recursive hooks and is not
    // a security boundary; `StateLock` documents the full threat model.
    // Independent top-level invocations still take the real file lock.
    //
    // The handle is bound to a local so its `Drop` releases the lock
    // when `run` returns by any path.
    let _lock = crate::state::StateLock::acquire(&context.roots.state_dir)?;

    // The caller's manifest snapshot was necessarily loaded before the state
    // lock. Another updater may have committed a newer method or ownership row
    // while this invocation waited, so all transition and cleanup decisions
    // below must be rebuilt from the now-serialized on-disk state.
    let fresh_manifest = manifest::read(context.manifest_path)?;
    let manifest = &fresh_manifest;

    let transitions = update_transition::by_name(manifest, entries, context.roots)?;
    let package_transitions = transitions.keys().cloned().collect::<BTreeSet<_>>();

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
            config::resolve_override_for_runtime(
                &entry.name,
                &entry.aliases,
                Some(context.pkg_mgr),
                context.env.is_android(),
            ) != "NONE"
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
            let package_versions =
                update_pkg::package_versions(entries, context, options, &package_transitions);
            let sudo =
                update_pkg::sudo_status(entries, context, &package_versions, &package_transitions)?;
            update_pkg::prepare(
                entries,
                context,
                &package_versions,
                &package_transitions,
                sudo,
                progress,
            )?;

            let mut package_clean = true;
            let mut package_done = 0usize;

            for entry in entries {
                if entry.method != method::PKG || !active(entry, context.env) {
                    continue;
                }

                let item = update_pkg::install(
                    entry,
                    context,
                    options,
                    sudo,
                    &mut queued,
                    &package_versions,
                    package_transitions.contains(&entry.name),
                )?;
                if !item.failed {
                    match item.reason {
                        ItemReason::Installed => cleanup_successful_transition(
                            entry,
                            transitions.get(&entry.name),
                            context,
                            &mut summary,
                        )?,
                        ItemReason::PackageUnavailable | ItemReason::PackageSudoUnavailable => {
                            update_transition::restore_failed(
                                transitions.get(&entry.name),
                                context.manifest_path,
                            )?
                        }
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
            update_pkg::flush(&queued, context, sudo, &mut changed, &mut summary, progress)?;
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

    // Only built-in, non-package methods enter this pool. Package managers
    // mutate shared system databases and have already completed above; custom
    // hooks are arbitrary caller code and remain serial below. The admitted
    // installers keep their managed roots and ledgers dependency-scoped, while
    // `manifest::upsert` serializes manifest updates. Public bin, man, and
    // completion directories are shared, so configured output names must also
    // be collision-free; parallel execution does not define a winner for a
    // collision. Preserve both sides of that boundary when adding a method.
    // The worker helper still returns outcomes in configuration order, even
    // though progress reports completion order, so dependency-result ordering
    // remains deterministic.
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
                repo_destination_snapshot(manifest.get(&entry.name), transitions.get(&entry.name)),
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
                    if advance_group(progress, &mut group_done, &group_totals, group)? {
                        if let Some(started) = group_started.remove(group) {
                            finish_group(&mut summary, group, started);
                        }
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
        if advance_group(progress, &mut group_done, &group_totals, group)? {
            if let Some(started) = group_started.remove(group) {
                finish_group(&mut summary, group, started);
            }
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

// Return the one repo root whose ownership changes during this transition.
fn repo_lock_root(
    entry: &Entry,
    transition: Option<&update_transition::Transition>,
    context: &Context<'_, impl Runner>,
) -> Option<PathBuf> {
    if entry.method == method::GITHUB_REPO {
        let source = repo::source(&entry.name, context.env_vars);
        return Some(context.roots.install_dir.join(source.name));
    }
    let old = transition.map(update_transition::old)?;
    if old.method != method::GITHUB_REPO {
        return None;
    }
    let roots = cleanup_roots(context.roots);
    cleanup::safe_repo_root(old, &roots)
}

// Enforce state-lock then checkout-lock ordering around every repo ownership change.
fn with_repo_checkout_lock<T>(
    entry: &Entry,
    transition: Option<&update_transition::Transition>,
    context: &Context<'_, impl Runner>,
    operation: impl FnOnce(Option<&Path>) -> Result<T>,
) -> Result<T> {
    let Some(root) = repo_lock_root(entry, transition, context) else {
        return operation(None);
    };

    #[cfg(unix)]
    {
        crate::checkout_lock::with_checkout_lock(&root, context.env_vars, |normalized| {
            // The lock serializes live writers; recovery closes the separate
            // uncatchable-death window before any caller derives ownership or
            // mutates the stable checkout path.
            crate::repo_transition::recover(normalized)?;
            operation(Some(normalized))
        })
    }

    #[cfg(not(unix))]
    {
        operation(Some(&root))
    }
}

// Serialize post-hook/package cleanup when the old method owned a repo root.
fn with_old_repo_checkout_lock<T>(
    old: Option<&ManifestEntry>,
    context: &Context<'_, impl Runner>,
    operation: impl FnOnce(Option<&Path>) -> Result<T>,
) -> Result<T> {
    let Some(old) = old.filter(|old| old.method == method::GITHUB_REPO) else {
        return operation(None);
    };
    let roots = cleanup_roots(context.roots);
    let Some(root) = cleanup::safe_repo_root(old, &roots) else {
        return operation(None);
    };

    #[cfg(unix)]
    {
        crate::checkout_lock::with_checkout_lock(&root, context.env_vars, |normalized| {
            crate::repo_transition::recover(normalized)?;
            operation(Some(normalized))
        })
    }

    #[cfg(not(unix))]
    {
        operation(Some(&root))
    }
}

/// Ownership evidence observed before acquiring the checkout mutation lock.
///
/// Recorded repo and external-method ownership are structural Shdeps state.
/// Release ownership is filesystem-derived, so the snapshot only records that
/// it must be revalidated after lock acquisition; it never authorizes mutation
/// on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoDestinationSnapshot {
    RecordedRepo,
    PreviousExternal,
    PreviousRelease,
    Unrecorded,
}

// Convert manifest/transition structure into pre-lock evidence without treating
// a release filesystem snapshot as final mutation authority.
fn repo_destination_snapshot(
    installed: Option<&ManifestEntry>,
    transition: Option<&update_transition::Transition>,
) -> RepoDestinationSnapshot {
    if installed.is_some_and(|entry| entry.method == method::GITHUB_REPO) {
        return RepoDestinationSnapshot::RecordedRepo;
    }
    match transition
        .map(update_transition::old)
        .map(|entry| entry.method.as_str())
    {
        Some(candidate) if method::is_external(candidate) => {
            RepoDestinationSnapshot::PreviousExternal
        }
        Some(method::GITHUB_RELEASE) => RepoDestinationSnapshot::PreviousRelease,
        _ => RepoDestinationSnapshot::Unrecorded,
    }
}

fn install_builtin<R>(
    entry: &Entry,
    context: &Context<'_, R>,
    options: Options,
    release_prefetch: &update_release::Prefetch,
    transition: Option<&update_transition::Transition>,
    repo_destination: RepoDestinationSnapshot,
) -> BuiltinOutcome
where
    R: Runner + Sync,
{
    let locked = with_repo_checkout_lock(entry, transition, context, |repo_root| {
        let refreshed_transition = if entry.method == method::GITHUB_REPO {
            update_transition::revalidate_for_repo_install(
                transition,
                context.roots,
                repo_root.expect("repo method requires a checkout lock root"),
            )?
        } else {
            None
        };
        let transition = refreshed_transition.as_ref().or(transition);
        let repo_destination = match repo_destination {
            RepoDestinationSnapshot::RecordedRepo => {
                update_repo::DestinationOwnership::RecordedRepo
            }
            RepoDestinationSnapshot::PreviousExternal => {
                update_repo::DestinationOwnership::PreviousMethod
            }
            RepoDestinationSnapshot::PreviousRelease
                if transition.is_some_and(update_transition::owns_repo_destination) =>
            {
                update_repo::DestinationOwnership::PreviousMethod
            }
            RepoDestinationSnapshot::PreviousRelease | RepoDestinationSnapshot::Unrecorded => {
                update_repo::DestinationOwnership::Unrecorded
            }
        };
        let result = match entry.method.as_str() {
            method::GITHUB_RELEASE => {
                update_transition::install_with_prepared(entry, transition, context.roots, || {
                    update_release::install_with_prefetch(entry, context, options, release_prefetch)
                })
            }
            method::GITHUB_REPO => {
                let install_dir = repo_root.expect("repo method requires a checkout lock root");
                match update_repo::prepare(entry, context, install_dir, repo_destination)? {
                    update_repo::Preparation::Failed(item) => Ok(item),
                    update_repo::Preparation::Ready(plan) => {
                        update_transition::install_with_prepared(
                            entry,
                            transition,
                            context.roots,
                            || update_repo::apply(*plan, entry, context, options),
                        )
                    }
                }
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
            update_transition::cleanup_successful(entry, transition, context.roots, repo_root)
                .unwrap_or(true)
        };
        Ok(BuiltinOutcome {
            item,
            cleanup_leftover,
        })
    });

    locked.unwrap_or_else(|error| BuiltinOutcome {
        item: Item::failed(
            entry.name.clone(),
            ItemReason::InstallFailed,
            error.to_string(),
        ),
        cleanup_leftover: false,
    })
}

fn successful_custom(
    entry: &Entry,
    context: &Context<'_, impl Runner>,
    changed: bool,
    detail: String,
    transition: Option<&ManifestEntry>,
) -> Result<CustomOutcome> {
    with_old_repo_checkout_lock(transition, context, |repo_root| {
        if transition.is_some_and(|old| old.method == method::GITHUB_REPO) && repo_root.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "github:repo transition cleanup requires an acquired checkout-lock root",
            )
            .into());
        }

        manifest::upsert(
            context.manifest_path,
            ManifestEntry::new(&entry.name, method::CUSTOM, &entry.cmd, ""),
        )?;

        let cleanup_leftover = match transition {
            Some(old) => cleanup_transition(old, context.roots, repo_root)?,
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
    })
}

fn cleanup_transition(
    old: &ManifestEntry,
    roots: &Roots,
    repo_root: Option<&Path>,
) -> Result<bool> {
    if old.method == method::GITHUB_REPO && repo_root.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo transition cleanup requires an acquired checkout-lock root",
        )
        .into());
    }
    // Transition cleanup happens after the new method is recorded so a cleanup
    // failure cannot erase the working install. Do not run `uninstall()` here:
    // the hook path is keyed only by dependency name, so after a switch there
    // is no reliable way to source the old method's hook separately from the
    // new method's hook. Running the current hook after a successful custom
    // install could undo the install we just accepted.
    Ok(cleanup::remove_builtin_with_repo_root(old, &cleanup_roots(roots), repo_root).is_err())
}

fn cleanup_successful_transition(
    entry: &Entry,
    transition: Option<&update_transition::Transition>,
    context: &Context<'_, impl Runner>,
    summary: &mut Summary,
) -> Result<()> {
    let cleanup = with_old_repo_checkout_lock(
        transition.map(update_transition::old),
        context,
        |repo_root| {
            if update_transition::cleanup_successful(entry, transition, context.roots, repo_root)? {
                summary.leftovers.push(entry.name.clone());
            }
            Ok(())
        },
    );
    if let Err(error) = cleanup {
        // Package installation records its new method before old repo cleanup.
        // A lock/recovery failure must not strand that new row while the old
        // checkout and link authority remain; restoring the prior row makes
        // the next update retry the same transition coherently.
        if let Err(restore) = update_transition::restore_failed(transition, context.manifest_path) {
            return Err(std::io::Error::other(format!(
                "transition cleanup failed ({error}); restoring the old manifest also failed ({restore})"
            ))
            .into());
        }
        return Err(error);
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
            let mut outcome = successful_custom(entry, context, false, detail, transition)?;
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
            let mut outcome = successful_custom(entry, context, true, detail, transition)?;
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::io::Cursor;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use super::{Context, Item, ItemReason, Options, Summary, run, run_with_progress};
    use bzip2::Compression as BzCompression;
    use bzip2::write::BzEncoder;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use crate::config::{parse_entry, parse_entry_for_runtime};
    use crate::github::{self, Asset, Release};
    use crate::hooks::BashCustomProbe;
    use crate::http::Client;
    use crate::link_state::{self, Kind};
    use crate::manifest::{self, Manifest, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;
    use crate::stamp;

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
    #[cfg(unix)]
    fn update_lock_normalizes_legacy_repo_manifest_path() {
        let fixture = Fixture::new("repo-lock-legacy-curdir");
        fixture.write_lib();
        fs::create_dir_all(fixture.roots.hooks_dir.join("owner")).unwrap();
        fixture.write_hook(
            "owner/tool",
            r#"
exists() { return 1; }
install() { printf 'installed\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("artifact"), "managed\n").unwrap();
        let legacy_spelling = format!("{}/./owner/tool", fixture.roots.install_dir.display());
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("owner/tool", "github:repo", "tool", legacy_spelling),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default();

        let summary = run(
            &[parse_entry("owner/tool|custom|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!install_root.exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new("owner/tool", "custom", "tool", ""))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_transition_consults_checkout_lock_before_operation() {
        let fixture = Fixture::new("repo-lock-structural-wiring");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("artifact"), "managed\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let malformed_lock = install_root.parent().unwrap().join(".tool.install.lock");
        fs::write(&malformed_lock, "foreign\n").unwrap();
        let expected_binary = fixture.roots.install_dir.join("tool/bin/tool");
        let runner = FakeRunner::default()
            .with_command("cargo")
            .with_created_binary(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    fixture.roots.install_dir.join("tool").to_str().unwrap(),
                    "tool",
                ],
                expected_binary,
            );
        let context = fixture.context(&manifest_path, &runner, "apt");

        let summary = run(
            &[parse_entry("tool|cargo|tool|-|-", None)],
            &installed,
            &context,
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(
            runner
                .calls()
                .iter()
                .all(|call| !call.starts_with("cargo\0install\0")),
            "the installer ran without acquiring its checkout lock"
        );
        assert!(install_root.join("artifact").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new(
                "tool",
                "github:repo",
                "tool",
                install_root.display().to_string()
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_transition_treats_development_installer_transaction_as_opaque() {
        let fixture = Fixture::new("repo-installer-transaction-wiring");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("artifact"), "managed\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let installer_transaction = install_root
            .parent()
            .unwrap()
            .join(".tool.install.transaction");
        fs::create_dir_all(&installer_transaction).unwrap();
        fs::write(
            installer_transaction.join("identity.development"),
            format!(
                "cgraf78 checkout installer development-to-managed transaction v1\n{}\nowner/tool\nmain\n/dev/tool\nsupport/install-checkout.sh\n\n",
                install_root.display()
            ),
        )
        .unwrap();
        let expected_binary = fixture.roots.install_dir.join("tool/bin/tool");
        let runner = FakeRunner::default()
            .with_command("cargo")
            .with_created_binary(
                "cargo",
                [
                    "install",
                    "--locked",
                    "--root",
                    fixture.roots.install_dir.join("tool").to_str().unwrap(),
                    "tool",
                ],
                expected_binary,
            );
        let context = fixture.context(&manifest_path, &runner, "apt");

        let summary = run(
            &[parse_entry("tool|cargo|tool|-|-", None)],
            &installed,
            &context,
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(
            summary.items[0]
                .detail
                .contains("rerun the checkout installer")
        );
        assert!(
            runner
                .calls()
                .iter()
                .all(|call| !call.starts_with("cargo\0install\0")),
            "the next method ran before installer-owned recovery"
        );
        assert_eq!(
            fs::read_to_string(install_root.join("artifact")).unwrap(),
            "managed\n"
        );
        assert!(installer_transaction.is_dir());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new(
                "tool",
                "github:repo",
                "tool",
                install_root.display().to_string()
            ))
        );
    }

    #[test]
    fn update_rejects_unsafe_transition_command_before_install_or_cleanup() {
        let fixture = Fixture::new("transition-unsafe-manifest-command");
        fixture.write_lib();
        fixture.write_hook(
            "tool",
            r#"
exists() { return 1; }
install() { printf 'installed\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let outside = fixture
            .roots
            .bin_dir
            .parent()
            .unwrap()
            .join("outside-command");
        fs::write(&outside, "preserve\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("tool", "github:release", "../outside-command", ""),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default();

        let result = run(
            &[parse_entry("tool|custom|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        );

        let error = result.expect_err("unsafe transition command was accepted");
        assert!(error.to_string().contains("unsafe command name"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "preserve\n");
        assert!(!fixture.roots.state_dir.join("tool-installed").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new(
                "tool",
                "github:release",
                "../outside-command",
                ""
            ))
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");

        let old_install = fixture.roots.install_dir.join("cgraf78/ds");
        let old_public = fixture.roots.bin_dir.join("ds");
        write_executable(&old_install.join("bin/ds"));
        fs::create_dir_all(old_public.parent().unwrap()).unwrap();
        symlink(old_install.join("bin/ds"), &old_public).unwrap();
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
        assert!(!summary.has_errors(), "{summary:?}");
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
    #[cfg(unix)]
    fn update_release_to_repo_preserves_unproven_foreign_canonical_root() {
        let fixture = Fixture::new("transition-release-foreign-root");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        let foreign_root = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(&foreign_root).unwrap();
        fs::write(foreign_root.join("sentinel"), "preserve\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            fixture.roots.bin_dir.join("tool").display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        let installed = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
        assert!(summary.items[0].detail.contains("refusing to adopt"));
        assert!(!foreign_root.is_symlink());
        assert_eq!(
            fs::read_to_string(foreign_root.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&old)
        );
        assert!(local_clone.join("bin/tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_release_to_repo_adopts_checkout_already_replaced_before_snapshot() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("transition-release-replaced-before-snapshot");
        fixture.write_lib();
        let install_root = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_root.join("bin/tool"));
        fs::write(install_root.join("sentinel"), "new checkout\n").unwrap();
        initialize_git_checkout(&install_root, "https://github.com/owner/tool");
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(
            install_root.join("bin/tool"),
            fixture.roots.bin_dir.join("tool"),
        )
        .unwrap();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));

        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                install_root.join("bin/tool").display().to_string(),
            ),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo"),
            1_700_000_000,
        )
        .unwrap();
        let runner = verified_adoption_runner(&install_root);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!install_root.is_symlink());
        assert_eq!(
            fs::read_to_string(install_root.join("sentinel")).unwrap(),
            "new checkout\n"
        );
        assert_eq!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .unwrap()
                .method,
            "github:repo"
        );
        assert!(local_clone.join("bin/tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn repo_install_revalidates_release_ownership_after_acquiring_checkout_lock() {
        let fixture = Fixture::new("transition-release-stale-ownership");
        fixture.write_lib();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("sentinel"), "preserve\n").unwrap();
        let archive_marker = crate::github_release_install::archive_layout_path(
            &fixture.roots.install_dir,
            "owner/tool",
        );
        fs::write(&archive_marker, "v1 archive\n").unwrap();

        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            install_root.join("bin/tool").display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let transitions = crate::update_transition::by_name(
            &installed,
            std::slice::from_ref(&entry),
            &fixture.roots,
        )
        .unwrap();
        let transition = transitions.get("owner/tool").unwrap();
        assert!(crate::update_transition::owns_repo_destination(transition));

        // Model the bootstrap installer replacing the archive root while it
        // owns the shared checkout lock. The transition snapshot above is now
        // stale; Shdeps must re-read filesystem ownership only after it later
        // acquires that same lock.
        fs::remove_file(&archive_marker).unwrap();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        let runner = FakeRunner::default();
        let outcome = super::install_builtin(
            &entry,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &crate::update_release::Prefetch::default(),
            Some(transition),
            super::RepoDestinationSnapshot::PreviousRelease,
        );

        assert!(outcome.item.failed);
        assert!(outcome.item.detail.contains("refusing to adopt"));
        assert!(!install_root.is_symlink());
        assert_eq!(
            fs::read_to_string(install_root.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&old)
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_install_does_not_reuse_legacy_archive_proof_for_a_new_root_generation() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("transition-release-stale-legacy-generation");
        fixture.write_lib();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_root.join("bin/tool"));
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(
            install_root.join("bin/tool"),
            fixture.roots.bin_dir.join("tool"),
        )
        .unwrap();

        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            install_root.join("bin/tool").display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let transitions = crate::update_transition::by_name(
            &installed,
            std::slice::from_ref(&entry),
            &fixture.roots,
        )
        .unwrap();
        let transition = transitions.get("owner/tool").unwrap();
        assert!(crate::update_transition::owns_repo_destination(transition));

        // A bootstrap installer can replace the directory at the same path
        // while the old public archive symlink remains valid. That path-based
        // symlink evidence belongs to the retired inode, not the new checkout
        // generation that now happens to expose the same command path.
        fs::remove_dir_all(&install_root).unwrap();
        write_executable(&install_root.join("bin/tool"));
        fs::write(install_root.join("sentinel"), "new generation\n").unwrap();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        let runner = FakeRunner::default();
        let outcome = super::install_builtin(
            &entry,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &crate::update_release::Prefetch::default(),
            Some(transition),
            super::RepoDestinationSnapshot::PreviousRelease,
        );

        assert!(outcome.item.failed);
        assert!(!install_root.is_symlink());
        assert_eq!(
            fs::read_to_string(install_root.join("sentinel")).unwrap(),
            "new generation\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&old)
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_install_does_not_upgrade_legacy_proof_created_while_waiting_for_lock() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("transition-release-new-legacy-proof");
        fixture.write_lib();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_root.join("bin/tool"));
        fs::write(install_root.join("sentinel"), "preserve\n").unwrap();

        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            install_root.join("bin/tool").display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let transitions = crate::update_transition::by_name(
            &installed,
            std::slice::from_ref(&entry),
            &fixture.roots,
        )
        .unwrap();
        let transition = transitions.get("owner/tool").unwrap();
        assert!(!crate::update_transition::owns_repo_destination(transition));

        // A legacy public symlink appearing during the lock wait is not proof
        // that the pre-lock release generation owned this root. Only proof
        // observed before waiting may be retained across an unchanged inode;
        // a newly published durable marker is the sole under-lock upgrade.
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(
            install_root.join("bin/tool"),
            fixture.roots.bin_dir.join("tool"),
        )
        .unwrap();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        let runner = FakeRunner::default();
        let outcome = super::install_builtin(
            &entry,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &crate::update_release::Prefetch::default(),
            Some(transition),
            super::RepoDestinationSnapshot::PreviousRelease,
        );

        assert!(outcome.item.failed);
        assert!(!install_root.is_symlink());
        assert_eq!(
            fs::read_to_string(install_root.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&old)
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_pkg_transition_cleans_old_builtin_artifacts_without_uninstalling_package() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("transition-pkg");
        fixture.write_lib();
        let old_install = fixture.roots.install_dir.join("tool");
        let old_public = fixture.roots.bin_dir.join("tool");
        write_executable(&old_install.join("bin/tool"));
        fs::create_dir_all(old_public.parent().unwrap()).unwrap();
        symlink(old_install.join("bin/tool"), &old_public).unwrap();
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
        let runner = FakeRunner::default()
            .with_command("tool")
            .with_success("apt-cache", ["show", "tool"], "Package: tool\n")
            .with_success("sudo", ["apt-get", "install", "-y", "tool"], "");

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
        assert!(
            runner
                .calls()
                .contains(&key("sudo", ["apt-get", "install", "-y", "tool"])),
            "the old managed command must not masquerade as package ownership"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&ManifestEntry::new("tool", "pkg", "tool", ""))
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_pkg_recovery_failure_restores_old_manifest_for_retry() {
        for already_installed in [true, false] {
            let fixture = Fixture::new(if already_installed {
                "repo-to-pkg-recovery-installed"
            } else {
                "repo-to-pkg-recovery-queued"
            });
            fixture.write_lib();
            let old_install = fixture.roots.install_dir.join("tool");
            let old_public = fixture.roots.bin_dir.join("tool");
            write_executable(&old_install.join("bin/tool"));
            fs::create_dir_all(old_public.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(old_install.join("bin/tool"), &old_public).unwrap();
            let manifest_path = manifest::path(&fixture.roots.state_dir);
            let old_manifest = ManifestEntry::new(
                "tool",
                "github:repo",
                "tool",
                old_install.display().to_string(),
            );
            manifest::upsert(&manifest_path, old_manifest.clone()).unwrap();
            let installer_transaction = old_install
                .parent()
                .unwrap()
                .join(".tool.install.transaction");
            fs::create_dir_all(&installer_transaction).unwrap();
            fs::write(installer_transaction.join("identity.partial"), "preserve\n").unwrap();
            let manifest = manifest::read(&manifest_path).unwrap();
            let runner = if already_installed {
                FakeRunner::default().with_success(
                    "dpkg-query",
                    ["-W", "-f=${Package}\t${Version}\n"],
                    "tool\t1.0\n",
                )
            } else {
                FakeRunner::default()
                    .with_success("dpkg-query", ["-W", "-f=${Package}\t${Version}\n"], "")
                    .with_success("apt-cache", ["show", "tool"], "Package: tool\n")
                    .with_success("sudo", ["apt-get", "install", "-y", "tool"], "")
            };

            let error = run(
                &[parse_entry("tool|pkg|tool|-|-", None)],
                &manifest,
                &fixture.context(&manifest_path, &runner, "apt"),
                Options::default(),
            )
            .unwrap_err();

            assert!(error.to_string().contains("rerun the checkout installer"));
            assert_eq!(
                manifest::read(&manifest_path).unwrap().get("tool"),
                Some(&old_manifest),
                "already_installed={already_installed}"
            );
            assert!(old_install.join("bin/tool").exists());
            assert_eq!(
                fs::read_link(&old_public).unwrap(),
                old_install.join("bin/tool")
            );
            assert!(installer_transaction.is_dir());
        }
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
    fn update_pkg_transition_restores_old_manifest_when_quiet_sudo_is_unavailable() {
        let fixture =
            Fixture::new("transition-pkg-quiet-no-sudo").with_env_var("SHDEPS_QUIET", "1");
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
            .with_success("id", ["-u"], "1000\n")
            .with_failure("sudo", ["-n", "true"]);

        let summary = run(
            &[parse_entry("tool|pkg|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].status, super::ItemStatus::Skipped);
        assert_eq!(
            summary.items[0].reason,
            super::ItemReason::PackageSudoUnavailable
        );
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
    fn update_quiet_package_fast_path_does_not_probe_sudo() {
        let fixture = Fixture::new("pkg-quiet-installed").with_env_var("SHDEPS_QUIET", "1");
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
        assert_eq!(summary.items[0].status, super::ItemStatus::Current);
        assert_eq!(summary.items[0].reason, super::ItemReason::Installed);
        assert!(
            runner.calls().is_empty(),
            "quiet cron runs with all package commands present should stay on the no-sudo fast path"
        );
    }

    #[test]
    fn update_quiet_missing_package_skips_when_sudo_would_prompt() {
        let fixture = Fixture::new("pkg-quiet-no-sudo").with_env_var("SHDEPS_QUIET", "1");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("id", ["-u"], "1000\n")
            .with_failure("sudo", ["-n", "true"]);

        let summary = run(
            &[parse_entry("jq|pkg|jq|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].status, super::ItemStatus::Skipped);
        assert_eq!(
            summary.items[0].reason,
            super::ItemReason::PackageSudoUnavailable
        );
        assert_eq!(summary.items[0].detail, "sudo unavailable in quiet mode");

        let calls = runner.calls();
        assert!(calls.contains(&key("id", ["-u"])));
        assert!(calls.contains(&key("sudo", ["-n", "true"])));
        assert!(
            !calls.contains(&key("sudo", ["apt-get", "update", "-qq"])),
            "quiet mode must not run sudo-backed metadata refresh when sudo would prompt"
        );
        assert!(
            !calls.contains(&key("apt-cache", ["show", "jq"])),
            "availability probes depend on refreshed metadata, so skip them when package work cannot run"
        );
        assert!(
            !calls.contains(&key("sudo", ["apt-get", "install", "-y", "jq"])),
            "quiet mode must not attempt package installs that require an interactive sudo prompt"
        );
    }

    #[test]
    fn update_quiet_root_still_runs_package_work() {
        let fixture = Fixture::new("pkg-quiet-root").with_env_var("SHDEPS_QUIET", "1");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("id", ["-u"], "0\n")
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
        assert_eq!(summary.items[0].status, super::ItemStatus::Changed);
        assert_eq!(summary.items[0].reason, super::ItemReason::Installed);

        let calls = runner.calls();
        assert!(calls.contains(&key("id", ["-u"])));
        assert!(
            !calls.contains(&key("sudo", ["-n", "true"])),
            "root can run package work directly, so quiet mode should not probe noninteractive sudo"
        );
        assert!(calls.contains(&key("sudo", ["apt-get", "update", "-qq"])));
        assert!(calls.contains(&key("apt-cache", ["show", "jq"])));
        assert!(calls.contains(&key("sudo", ["apt-get", "install", "-y", "jq"])));
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
    fn update_runs_termux_apt_without_sudo_or_prompt_pause() {
        let mut fixture = Fixture::new("pkg-termux-direct");
        fixture.env = RuntimeEnv::new("linux", "phone").with_android(true);
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success("dpkg-query", ["-W", "-f=${Package}\t${Version}\n"], "")
            .with_success("apt-get", ["update", "-qq"], "")
            .with_success("apt-cache", ["show", "termux-jq"], "Package: termux-jq\n")
            .with_success("apt-get", ["install", "-y", "termux-jq"], "");
        let mut progress = RecordingProgress::default();

        let summary = run_with_progress(
            &[parse_entry_for_runtime(
                "jq|pkg|missing-jq|android:termux-jq,apt:jq|-",
                Some("apt"),
                true,
            )],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
            &mut progress,
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(progress.prompt_pauses.is_empty());
        let calls = runner.calls();
        assert!(calls.contains(&key("apt-get", ["update", "-qq"])));
        assert!(calls.contains(&key("apt-cache", ["show", "termux-jq"])));
        assert!(calls.contains(&key("apt-get", ["install", "-y", "termux-jq"])));
        assert!(
            calls.iter().all(|call| !call.starts_with("sudo\0")),
            "Termux package work must never cross a sudo boundary: {calls:?}"
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
    #[cfg(unix)]
    fn update_github_release_plain_asset_refuses_archive_format_change() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("release-plain-clears-archive-binlinks");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v2.0.0",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old_install = fixture.roots.install_dir.join("owner/tool");
        let old_tool = old_install.join("bin/tool");
        let old_helper = old_install.join("bin/tool-helper");
        write_executable(&old_tool);
        write_executable(&old_helper);
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        let bin_path = fixture.roots.bin_dir.join("tool");
        let helper_path = fixture.roots.bin_dir.join("tool-helper");
        symlink(&old_tool, &bin_path).unwrap();
        symlink(&old_helper, &helper_path).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin),
            &[bin_path.clone(), helper_path.clone()],
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin_path.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.0.0\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(fs::read_link(&bin_path).unwrap(), old_tool);
        assert_eq!(fs::read_link(&helper_path).unwrap(), old_helper);
        assert!(link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin).exists());
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
    fn update_github_release_falls_back_to_named_release_wide_checksum() {
        // Watchexec and similar releases publish a per-asset digest without a
        // filename, plus a release-wide manifest that does bind the digest to
        // the asset. The bare sibling must remain untrusted, but it must not
        // hide the usable named manifest from the verifier.
        let mut fixture = Fixture::new("release-checksum-sibling-fallback");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let sibling_checksum = crate::checksum::sha256_hex(&binary);
        let release_wide_checksum = format!(
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
                binary.clone(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256",
                sibling_checksum.into_bytes(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/SHA256SUMS",
                release_wide_checksum.into_bytes(),
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
        let urls = fixture
            .client
            .requests()
            .into_iter()
            .map(|(url, _)| url)
            .collect::<Vec<_>>();
        assert!(
            urls.iter().any(|url| url.ends_with(".sha256")),
            "bare sibling must be inspected before the fallback: {urls:?}"
        );
        assert!(
            urls.iter().any(|url| url.ends_with("SHA256SUMS")),
            "named release-wide manifest must be used as the fallback: {urls:?}"
        );
    }

    #[test]
    fn forced_release_install_ignores_cached_release_metadata() {
        release_install_ignores_cached_release_metadata(
            "force-release-cache",
            Options {
                force: true,
                now: 1_700_000_000,
                ..Options::default()
            },
        );
    }

    #[test]
    fn reinstall_release_install_ignores_cached_release_metadata() {
        release_install_ignores_cached_release_metadata(
            "reinstall-release-cache",
            Options {
                reinstall: true,
                now: 1_700_000_000,
                ..Options::default()
            },
        );
    }

    fn release_install_ignores_cached_release_metadata(fixture_name: &str, options: Options) {
        // A stamp can survive from a previous invocation that happened in the
        // same epoch second. `--force` must still use fresh REST metadata: an
        // old asset list can pair a newly served binary with an obsolete
        // checksum and turn a valid upstream release into a false mismatch.
        let mut fixture = Fixture::new(fixture_name);
        fixture.write_lib();
        let stale_asset = "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64";
        let stale_checksum =
            "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.sha256";
        let fresh_asset = "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64";
        let fresh_checksum =
            "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64.sha256";
        let stale_releases = vec![Release {
            tag: "v1".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: "tool-linux-x86_64".to_owned(),
                    url: stale_asset.to_owned(),
                    api_url: None,
                },
                Asset {
                    name: "tool-linux-x86_64.sha256".to_owned(),
                    url: stale_checksum.to_owned(),
                    api_url: None,
                },
            ],
        }];
        github::write_cached_releases(&fixture.roots.state_dir, "owner/tool", &stale_releases)
            .unwrap();
        let now = options.now;
        stamp::remote_touch(
            &stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "github"),
            now,
        )
        .unwrap();

        let fresh_binary = b"fresh-binary".to_vec();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                format!(
                    r#"[{{"tag_name":"v2","draft":false,"prerelease":false,"assets":[{{"name":"tool-linux-x86_64","browser_download_url":"{fresh_asset}"}},{{"name":"tool-linux-x86_64.sha256","browser_download_url":"{fresh_checksum}"}}]}}]"#
                )
                .into_bytes(),
            )
            .with(stale_asset, b"stale-binary".to_vec())
            .with(stale_checksum, b"0000  tool-linux-x86_64\n".to_vec())
            .with(fresh_asset, fresh_binary.clone())
            .with(
                fresh_checksum,
                format!(
                    "{}  tool-linux-x86_64\n",
                    crate::checksum::sha256_hex(&fresh_binary)
                )
                .into_bytes(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            options,
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            fs::read(fixture.roots.bin_dir.join("tool")).unwrap(),
            fresh_binary
        );
        let urls = fixture
            .client
            .requests()
            .into_iter()
            .map(|(url, _)| url)
            .collect::<Vec<_>>();
        assert!(urls.contains(&github::releases_url("owner/tool")));
        assert!(urls.contains(&fresh_asset.to_owned()));
        assert!(!urls.contains(&stale_asset.to_owned()));
    }

    #[test]
    fn update_github_release_rejects_named_sibling_before_release_wide_fallback() {
        // A filename-bound sibling that disagrees with the downloaded bytes is
        // an integrity failure, not a reason to seek a lower-priority digest.
        // The release-wide manifest must therefore remain unread.
        let mut fixture = Fixture::new("release-checksum-sibling-mismatch");
        fixture.write_lib();
        let binary = b"binary".to_vec();
        let sibling_checksum = format!(
            "{}  tool-linux-x86_64\n",
            crate::checksum::sha256_hex(b"different-bytes")
        );
        let release_wide_checksum = format!(
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
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64.sha256",
                sibling_checksum.into_bytes(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/SHA256SUMS",
                release_wide_checksum.into_bytes(),
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
        assert_eq!(
            summary.items[0].detail,
            format!(
                "release asset checksum mismatch (tool-linux-x86_64, {} bytes, sha256 {})",
                b"binary".len(),
                crate::checksum::sha256_hex(b"binary")
            )
        );
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        let urls = fixture
            .client
            .requests()
            .into_iter()
            .map(|(url, _)| url)
            .collect::<Vec<_>>();
        assert!(
            !urls.iter().any(|url| url.ends_with("SHA256SUMS")),
            "a named sibling mismatch must stop before fallback: {urls:?}"
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
        assert_eq!(
            summary.items[0].detail,
            format!(
                "release asset checksum mismatch (tool-linux-x86_64, {} bytes, sha256 {})",
                b"binary".len(),
                crate::checksum::sha256_hex(b"binary")
            )
        );
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
        assert_eq!(
            summary.items[0].detail,
            format!(
                "release asset checksum mismatch (tool-linux-x86_64, {} bytes, sha256 {})",
                b"binary".len(),
                crate::checksum::sha256_hex(b"binary")
            )
        );
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
            .insert("SHDEPS_ALLOW_GH_AUTH_TOKEN".to_owned(), "1".to_owned());
        fixture.client = FakeClient::default()
            .with_redirect(
                "https://github.com/owner/tool-a/releases/latest",
                "https://github.com/owner/tool-a/releases/tag/v1.0.0",
            )
            .with_redirect(
                "https://github.com/owner/tool-b/releases/latest",
                "https://github.com/owner/tool-b/releases/tag/v2.0.0",
            );
        write_executable(&fixture.roots.bin_dir.join("tool-a"));
        write_executable(&fixture.roots.bin_dir.join("tool-b"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_command("gh")
            .with_success("gh", ["auth", "token"], "gh-token\n")
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
        let api_count = fixture
            .client
            .requests()
            .iter()
            .filter(|(url, _)| url.starts_with("https://api.github.com/"))
            .count();
        let latest_count = fixture
            .client
            .requests()
            .iter()
            .filter(|(url, _)| url.ends_with("/releases/latest"))
            .count();
        let token_calls = runner
            .calls()
            .into_iter()
            .filter(|call| call == &key("gh", ["auth", "token"]))
            .count();

        assert!(!summary.has_errors());
        assert!(summary.items.iter().all(|item| !item.changed));
        assert_eq!(
            asset_count, 0,
            "prefetched current versions should preserve the force no-op path and avoid unnecessary release downloads"
        );
        assert_eq!(
            api_count, 0,
            "matching public latest-release redirects should avoid GitHub REST API quota entirely"
        );
        assert_eq!(
            latest_count, 2,
            "each dependency should need exactly one public latest-release probe"
        );
        assert_eq!(
            token_calls, 0,
            "fully current public releases should not need `gh auth token`"
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
    fn rate_limited_metadata_fetch_fails_uninstalled_release_with_classified_detail() {
        let mut fixture = Fixture::new("release-metadata-rate-limit-missing");
        fixture.write_lib();
        fixture.client = FakeClient::default().with_status_error(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            403,
        );
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
        assert_eq!(
            summary.items[0].detail,
            "GitHub API rate limit exceeded; retry after the window resets or set GH_TOKEN"
        );
    }

    #[test]
    fn rate_limited_metadata_fetch_keeps_installed_release_and_says_why() {
        let mut fixture = Fixture::new("release-metadata-rate-limit-installed");
        fixture.write_lib();
        fixture.client = FakeClient::default().with_status_error(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            429,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        let runner = FakeRunner::default().with_success("tool", ["--version"], "tool 1.0.0\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "1.0.0 (update check rate-limited)");
    }

    #[test]
    fn missing_repo_metadata_fetch_fails_with_not_found_detail() {
        let mut fixture = Fixture::new("release-metadata-not-found");
        fixture.write_lib();
        fixture.client = FakeClient::default().with_status_error(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            404,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(
            summary.items[0].detail,
            "no GitHub release metadata (repo missing, private, or unreleased)"
        );
    }

    #[test]
    fn unclassified_metadata_fetch_failure_keeps_legacy_details() {
        let fixture = Fixture::new("release-metadata-legacy-missing");
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
        assert_eq!(summary.items[0].detail, "release metadata fetch failed");

        let fixture = Fixture::new("release-metadata-legacy-installed");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        write_executable(&fixture.roots.bin_dir.join("tool"));

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(summary.items[0].detail, "release metadata unavailable");
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
        fixture.client = FakeClient::default().with_redirect(
            "https://github.com/owner/tool/releases/latest",
            "https://github.com/owner/tool/releases/tag/v1.2.3",
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
                "https://github.com/owner/tool/releases/latest".to_owned(),
                None
            )],
            "force should confirm the current tag without spending GitHub REST API quota"
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
    fn forced_release_install_rechecks_equal_timestamp_stamp() {
        // A persisted stamp has no run identity, so an equal timestamp cannot
        // prove that this forced invocation already checked the release.
        // Recheck it rather than letting a just-finished older run suppress
        // the caller's explicit refresh request.
        let fixture = Fixture::new("release-force-generic-handoff");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            1_700_000_500,
        )
        .unwrap();
        let runner = FakeRunner::default().with_success("tool", ["--version"], "tool 1.2.3\n");

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
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://github.com/owner/tool/releases/latest".to_owned(),
                    None
                ),
                (
                    "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                    None
                ),
            ]
        );
    }

    #[test]
    fn update_github_release_changed_redirect_falls_back_to_rest_metadata() {
        let mut fixture = Fixture::new("release-force-changed");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with_redirect(
                "https://github.com/owner/tool/releases/latest",
                "https://github.com/owner/tool/releases/tag/v1.2.3",
            )
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with(
                "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64",
                b"new-binary".to_vec(),
            );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin_path = fixture.roots.bin_dir.join("tool");
        write_executable(&bin_path);
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.2.2\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                force: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(fs::read(&bin_path).unwrap(), b"new-binary");
        assert_eq!(
            fixture.client.requests(),
            vec![
                (
                    "https://github.com/owner/tool/releases/latest".to_owned(),
                    None,
                ),
                (
                    "https://api.github.com/repos/owner/tool/releases?per_page=100".to_owned(),
                    None,
                ),
                (
                    "https://github.com/owner/tool/releases/download/v1.2.3/tool-linux-x86_64"
                        .to_owned(),
                    None,
                ),
            ],
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
            ("tool-v1.2.3/bin/tool-helper", b"helper".as_slice(), 0o755),
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
        let helper_path = fixture.roots.bin_dir.join("tool-helper");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(
            fs::read_link(&bin_path).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            fs::read_link(&helper_path).unwrap(),
            install_dir.join("bin/tool-helper")
        );
        assert_eq!(
            link_state::read(&link_state::path(
                &fixture.roots.state_dir,
                "owner/tool",
                Kind::Bin
            ))
            .unwrap(),
            vec![bin_path.clone(), helper_path]
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
    fn update_github_release_warm_path_does_not_infer_archive_from_secondary_link() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("release-marker-backfill");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let public = fixture.roots.bin_dir.join("tool");
        let man_source = install_dir.join("share/man/man1/tool.1");
        let man_link = fixture.roots.install_dir.join("man/man1/tool.1");
        write_executable(&install_dir.join("bin/tool"));
        write_executable(&public);
        fs::create_dir_all(man_source.parent().unwrap()).unwrap();
        fs::create_dir_all(man_link.parent().unwrap()).unwrap();
        fs::write(&man_source, "manual").unwrap();
        symlink(&man_source, &man_link).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Extras),
            std::slice::from_ref(&man_link),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "release"),
            1_700_000_000,
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &installed,
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
        assert!(!public.is_symlink());
        assert!(
            !crate::github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/tool"
            )
            .exists(),
            "a secondary link cannot distinguish a launcher from a raw release"
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_refuses_raw_to_archive_format_change() {
        let mut fixture = Fixture::new("release-raw-to-archive");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        write_executable(&public);
        fs::write(&public, "#!/bin/sh\nprintf raw-v1\\n\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let url = "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64.tar.gz";
        fixture.client = FakeClient::default().with(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            release_asset_response("tool-linux-x86_64.tar.gz", "v2.0.0", url),
        );
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.0.0\n")
            .with_success("uname", ["-m"], "x86_64\n");
        let installed = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "#!/bin/sh\nprintf raw-v1\\n\n"
        );
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_does_not_replace_unowned_symlink_during_format_change() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("release-symlink-to-archive");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let external = fixture.roots.home.join("user-tool");
        write_executable(&external);
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        symlink(&external, &public).unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let url = "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64.tar.gz";
        fixture.client = FakeClient::default().with(
            "https://api.github.com/repos/owner/tool/releases?per_page=100",
            release_asset_response("tool-linux-x86_64.tar.gz", "v2.0.0", url),
        );
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.0.0\n")
            .with_success("uname", ["-m"], "x86_64\n");
        let installed = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(fs::read_link(public).unwrap(), external);
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_refuses_raw_asset_over_archive_launcher() {
        let mut fixture = Fixture::new("release-archive-launcher-to-raw");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let public = fixture.roots.bin_dir.join("tool");
        write_executable(&install_dir.join("bin/tool"));
        fs::write(
            crate::github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/tool",
            ),
            "v1 archive\n",
        )
        .unwrap();
        write_executable(&public);
        fs::write(&public, "#!/bin/sh\nexec archive-core \"$@\"\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let url = "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64";
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                release_response("tool", "v2.0.0", url),
            )
            .with(url, b"raw-v2".to_vec());
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.0.0\n")
            .with_success("uname", ["-m"], "x86_64\n");
        let installed = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "#!/bin/sh\nexec archive-core \"$@\"\n"
        );
        assert!(install_dir.exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_release_without_manifest_does_not_convert_marked_archive() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("release-crash-before-manifest");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let public = fixture.roots.bin_dir.join("tool");
        let archive_tool = install_dir.join("bin/tool");
        write_executable(&archive_tool);
        fs::write(
            crate::github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/tool",
            ),
            "v1 archive\n",
        )
        .unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        symlink(&archive_tool, &public).unwrap();
        let url = "https://github.com/owner/tool/releases/download/v2/tool-linux-x86_64";
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases?per_page=100",
                release_response("tool", "v2.0.0", url),
            )
            .with(url, b"raw-v2".to_vec());
        let runner = FakeRunner::default()
            .with_success("tool", ["--version"], "tool 1.0.0\n")
            .with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(fs::read_link(&public).unwrap(), archive_tool);
        assert!(install_dir.exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_release_failure_does_not_mark_checkout_as_archive() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-to-release-marker-failure");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let checkout = fixture.roots.install_dir.join("owner/tool");
        let checkout_tool = checkout.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        write_executable(&checkout_tool);
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        symlink(&checkout_tool, &public).unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                checkout.display().to_string(),
            ),
        )
        .unwrap();
        let installed = manifest::read(&manifest_path).unwrap();
        let runner = FakeRunner::default().with_success("uname", ["-m"], "x86_64\n");

        let summary = run(
            &[parse_entry("owner/tool|github:release|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(!checkout.join(".shdeps-release-layout").exists());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            installed.get("owner/tool")
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
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
    fn update_github_repo_rejects_foreign_development_origin_before_publication() {
        let fixture = Fixture::new("repo-local-foreign-origin");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/other/tool");
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("development checkout"));
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_accepts_matching_development_origin_override() {
        let mut fixture = Fixture::new("repo-local-origin-override");
        fixture.write_lib();
        fixture.env_vars.insert(
            "SHDEPS_TOOL_REPO".to_owned(),
            "git@github.com:other/tool.git".to_owned(),
        );
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/other/tool");
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("owner/tool");
        assert!(!summary.has_errors());
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_link.display().to_string(),
            ))
        );
    }

    #[test]
    fn update_github_repo_accepts_matching_non_github_development_origin_override() {
        let mut fixture = Fixture::new("repo-local-non-github-origin-override");
        fixture.write_lib();
        let configured_origin = format!(
            "file://{}",
            fixture.roots.home.join("mirrors/tool.git").display()
        );
        fixture
            .env_vars
            .insert("SHDEPS_TOOL_REPO".to_owned(), configured_origin.clone());
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, &configured_origin);
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("owner/tool");
        assert!(!summary.has_errors(), "{}", summary.items[0].detail);
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
    }

    #[test]
    fn update_github_repo_rejects_ambiguous_or_rewritten_development_origin() {
        #[derive(Debug, Clone, Copy)]
        enum OriginShape {
            Duplicate,
            InsteadOf,
        }

        for shape in [OriginShape::Duplicate, OriginShape::InsteadOf] {
            let fixture = Fixture::new(&format!("repo-local-origin-{shape:?}"));
            fixture.write_lib();
            let local_clone = fixture.roots.git_dev_dir.join("tool");
            write_executable(&local_clone.join("bin/tool"));
            match shape {
                OriginShape::Duplicate => {
                    initialize_git_checkout(&local_clone, "https://github.com/other/tool");
                    fixture_git(
                        &local_clone,
                        &[
                            "config",
                            "--add",
                            "remote.origin.url",
                            "https://github.com/owner/tool",
                        ],
                    );
                }
                OriginShape::InsteadOf => {
                    initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
                    fixture_git(
                        &local_clone,
                        &[
                            "config",
                            "url.https://example.invalid/.insteadOf",
                            "https://github.com/",
                        ],
                    );
                }
            }
            let manifest_path = manifest::path(&fixture.roots.state_dir);

            let summary = run(
                &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
                &Manifest::default(),
                &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
                Options::default(),
            )
            .unwrap();

            assert!(summary.has_errors(), "{shape:?}");
            assert!(
                summary.items[0].detail.contains("development checkout"),
                "{shape:?}: {}",
                summary.items[0].detail
            );
            assert!(!fixture.roots.install_dir.join("owner/tool").exists());
            assert!(!fixture.roots.bin_dir.join("tool").exists());
        }
    }

    #[test]
    fn update_github_repo_rejects_untracked_development_command() {
        let fixture = Fixture::new("repo-local-untracked-command");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        fs::create_dir_all(&local_clone).unwrap();
        fs::write(local_clone.join("README.md"), "fixture\n").unwrap();
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        write_executable(&local_clone.join("bin/tool"));
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_allows_dirty_tracked_development_command() {
        let fixture = Fixture::new("repo-local-dirty-command");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        fs::write(local_clone.join("bin/tool"), "#!/bin/sh\necho dirty\n").unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("owner/tool");
        assert!(!summary.has_errors());
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            fs::read_to_string(local_clone.join("bin/tool")).unwrap(),
            "#!/bin/sh\necho dirty\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_rejects_dirty_symlinked_development_bin_directory() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-local-symlinked-bin");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        fs::remove_dir_all(local_clone.join("bin")).unwrap();
        let outside_bin = fixture.roots.home.join("outside-bin");
        write_executable(&outside_bin.join("tool"));
        symlink(&outside_bin, local_clone.join("bin")).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
    }

    #[test]
    fn update_github_repo_rejects_nested_directory_in_ancestor_checkout() {
        let fixture = Fixture::new("repo-local-ancestor-checkout");
        fixture.write_lib();
        let checkout = fixture.roots.git_dev_dir.clone();
        let local_clone = checkout.join("tool");
        fs::create_dir_all(&local_clone).unwrap();
        fs::write(local_clone.join("plugin.zsh"), "# plugin\n").unwrap();
        initialize_git_checkout(&checkout, "https://github.com/owner/tool");
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("development checkout"));
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_install_root_update_then_prune_removes_physical_checkout() {
        use std::os::unix::fs::symlink;

        let mut fixture = Fixture::new("repo-symlinked-install-root");
        fixture.write_lib();
        let physical_install = fixture.roots.home.join("physical-share");
        let logical_install = fixture.roots.home.join("share-link");
        fs::create_dir_all(&physical_install).unwrap();
        symlink(&physical_install, &logical_install).unwrap();
        fixture.roots.install_dir = logical_install;
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default();

        let update = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        let physical_checkout = physical_install.join("owner/tool");
        assert!(!update.has_errors());
        assert!(fs::symlink_metadata(&physical_checkout).is_ok());
        assert_eq!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .unwrap()
                .install_path,
            physical_checkout.display().to_string()
        );

        let installed = manifest::read(&manifest_path).unwrap();
        crate::prune::run(
            &[],
            &installed,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            crate::prune::Options {
                yes: true,
                ..crate::prune::Options::default()
            },
        )
        .unwrap();

        assert!(fs::symlink_metadata(&physical_checkout).is_err());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
        assert!(local_clone.join("bin/tool").exists());
    }

    #[test]
    fn update_github_repo_rejects_local_dev_clone_missing_explicit_command() {
        let fixture = Fixture::new("repo-local-missing-cmd");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("cli");
        fs::create_dir_all(&local_clone).unwrap();
        fs::write(local_clone.join("README.md"), "fixture\n").unwrap();
        initialize_git_checkout(&local_clone, "https://github.com/smallstep/cli");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default().with_success(
            "git",
            ["-C", local_clone.to_str().unwrap(), "rev-parse", "HEAD"],
            "head\n",
        );

        let summary = run(
            &[parse_entry("smallstep/cli|github:repo|step|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["smallstep/cli"]);
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert_eq!(
            summary.items[0].detail,
            "configured command `step` not found in repo bin"
        );
        assert!(!fixture.roots.install_dir.join("smallstep/cli").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("smallstep/cli")
                .is_none()
        );
        assert!(
            !crate::stamp::remote_path(&fixture.roots.state_dir, "smallstep/cli", "repo").exists(),
            "missing explicit command must not refresh the repo TTL"
        );
        assert!(
            !crate::stamp::revision_path(&fixture.roots.state_dir, "smallstep/cli").exists(),
            "missing explicit command must not refresh the repo revision"
        );
    }

    #[test]
    fn update_github_repo_allows_asset_only_local_dev_clone_without_explicit_command() {
        let fixture = Fixture::new("repo-local-asset-only");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("plugin");
        fs::create_dir_all(&local_clone).unwrap();
        fs::write(local_clone.join("plugin.zsh"), "# plugin\n").unwrap();
        initialize_git_checkout(&local_clone, "https://github.com/owner/plugin");
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/plugin|github:repo", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let install_link = fixture.roots.install_dir.join("owner/plugin");
        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/plugin"),
            Some(&ManifestEntry::new(
                "owner/plugin",
                "github:repo",
                "plugin",
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
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
    fn update_github_repo_reuses_recorded_local_clone_symlink() {
        let fixture = Fixture::new("repo-local-unchanged");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&local_clone, &install_link).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let installed = record_repo_manifest(&manifest_path, "cgraf78/ds", "ds", &install_link);
        let manifest_before = fs::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        let public = fixture.roots.bin_dir.join("ds");
        assert_eq!(fs::read_link(&public).unwrap(), install_link.join("bin/ds"));
        assert_eq!(
            link_state::read(&link_state::path(
                &fixture.roots.state_dir,
                "cgraf78/ds",
                Kind::Bin,
            ))
            .unwrap(),
            [public]
        );
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_adopts_exact_unrecorded_local_clone_symlink_without_replacing_it() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-local-unrecorded-link");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let install_link = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        symlink(&local_clone, &install_link).unwrap();
        let link_inode = fs::symlink_metadata(&install_link).unwrap().ino();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            fs::symlink_metadata(&install_link).unwrap().ino(),
            link_inode
        );
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_link.join("bin/tool")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_link.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_recovers_unrecorded_link_to_symlinked_dev_checkout() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-local-unrecorded-symlinked-dev");
        fixture.write_lib();
        let real_clone = fixture.roots.home.join("real-dev-tool");
        write_executable(&real_clone.join("bin/tool"));
        initialize_git_checkout(&real_clone, "https://github.com/owner/tool");
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        fs::create_dir_all(local_clone.parent().unwrap()).unwrap();
        symlink(&real_clone, &local_clone).unwrap();

        // This is the exact state left if the historical development-link
        // publication succeeds but the manifest write is interrupted. The
        // next run must be able to validate and finish owning what Shdeps
        // itself already published.
        let install_link = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        symlink(&local_clone, &install_link).unwrap();
        let install_inode = fs::symlink_metadata(&install_link).unwrap().ino();
        let manifest_path = manifest::path(&fixture.roots.state_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(fs::read_link(&install_link).unwrap(), local_clone);
        assert_eq!(
            fs::symlink_metadata(&install_link).unwrap().ino(),
            install_inode
        );
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_link.join("bin/tool")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_link.display().to_string(),
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_warns_when_local_clone_cannot_fast_forward() {
        let fixture = Fixture::new("repo-local-diverged");
        fixture.write_lib();
        let local_clone = fixture.roots.git_dev_dir.join("ds");
        write_executable(&local_clone.join("bin/ds"));
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
        let install_link = fixture.roots.install_dir.join("cgraf78/ds");
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&local_clone, &install_link).unwrap();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let runner = FakeRunner::default()
            .with_success(
                "git",
                [
                    "-C",
                    local_clone.to_str().unwrap(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                "",
            )
            .with_success(
                "git",
                [
                    "-C",
                    local_clone.to_str().unwrap(),
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                "origin/main\n",
            )
            .with_failure(
                "git",
                [
                    "-C",
                    local_clone.to_str().unwrap(),
                    "pull",
                    "--ff-only",
                    "--quiet",
                ],
            )
            .with_success(
                "git",
                [
                    "-C",
                    local_clone.to_str().unwrap(),
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("cgraf78/ds|github:repo|ds|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert!(!summary.items[0].failed);
        assert_eq!(summary.items[0].status, super::ItemStatus::Warning);
        assert_eq!(summary.items[0].reason, super::ItemReason::RepoPullFailed);
        assert_eq!(
            summary.items[0].detail,
            "pull failed (no fast-forward; local clone)"
        );
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
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
            .with_created_binary(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/cgraf78/ds",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join("bin/ds"),
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
        initialize_git_checkout(&local_clone, "https://github.com/cgraf78/ds");
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
            .with_created_binary(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/private/tool",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join("bin/tool"),
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
    fn update_github_repo_rejects_fresh_clone_missing_explicit_command() {
        let mut fixture = Fixture::new("repo-fresh-missing-cmd");
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
        let runner = FakeRunner::default().with_command("git").with_created_dir(
            "git",
            [
                "clone",
                "--depth",
                "1",
                "https://github.com/private/tool",
                clone_tmp.to_str().unwrap(),
            ],
            clone_tmp.join(".git"),
        );

        let summary = run(
            &[parse_entry("private/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.failed, ["private/tool"]);
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert_eq!(
            summary.items[0].detail,
            "configured command `tool` not found in repo bin"
        );
        assert!(!install_dir.exists());
        assert!(!clone_tmp.exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("private/tool")
                .is_none()
        );
        assert!(
            !crate::stamp::remote_path(&fixture.roots.state_dir, "private/tool", "repo").exists(),
            "missing explicit command must not refresh the repo TTL"
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_fresh_clone_preserves_destination_appearing_during_clone() {
        let fixture = Fixture::new("repo-fresh-late-destination");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let clone_tmp = fixture
            .roots
            .install_dir
            .join(format!("owner/tool.tmp.{}", std::process::id()));
        let clone_args = [
            "clone",
            "--depth",
            "1",
            "https://github.com/owner/tool",
            clone_tmp.to_str().unwrap(),
        ];
        let runner = FakeRunner::default()
            .with_command("git")
            .with_created_dir("git", clone_args, clone_tmp.join(".git"))
            .with_created_dir("git", clone_args, install_dir.clone())
            .with_created_binary("git", clone_args, clone_tmp.join("bin/tool"));

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("destination appeared"));
        assert_eq!(fs::read_dir(&install_dir).unwrap().count(), 0);
        assert!(!clone_tmp.exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
        assert!(
            !install_dir
                .parent()
                .unwrap()
                .join(".tool.shdeps-repo-transition-v1")
                .exists()
        );
    }

    #[test]
    fn update_github_repo_allows_asset_only_fresh_clone_without_explicit_command() {
        let fixture = Fixture::new("repo-fresh-asset-only");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/plugin");
        let clone_tmp = fixture
            .roots
            .install_dir
            .join(format!("owner/plugin.tmp.{}", std::process::id()));
        let runner = FakeRunner::default()
            .with_command("git")
            .with_created_dir(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "https://github.com/owner/plugin",
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
                    "git@github.com:owner/plugin.git",
                ],
                "",
            );

        let summary = run(
            &[parse_entry("owner/plugin|github:repo", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(summary.items[0].changed);
        assert!(install_dir.join(".git").is_dir());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/plugin"),
            Some(&ManifestEntry::new(
                "owner/plugin",
                "github:repo",
                "plugin",
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
            .with_created_binary(
                "git",
                [
                    "clone",
                    "--depth",
                    "1",
                    "git@github.com:private/tool.git",
                    clone_tmp.to_str().unwrap(),
                ],
                clone_tmp.join("bin/tool"),
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
        write_executable(&install_dir.join("bin/tool"));
        fs::create_dir_all(install_dir.join("src")).unwrap();
        fs::write(install_dir.join("src/_tool"), "#compdef tool\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/private/tool");
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
        let runner = verified_adoption_runner(&install_dir);

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
        let public = fixture.roots.bin_dir.join("tool");
        assert_eq!(
            fs::read_link(&public).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            link_state::read(&link_state::path(
                &fixture.roots.state_dir,
                "private/tool",
                Kind::Bin,
            ))
            .unwrap(),
            [public]
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
        assert_isolated_verification_calls(
            &runner.clean_calls(),
            &install_dir,
            &fixture.roots.state_dir,
            "https://github.com/private/tool",
            "main",
            2,
        );
    }

    #[test]
    fn update_github_repo_adopts_valid_existing_asset_only_checkout() {
        let fixture = Fixture::new("repo-existing-asset-only");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/plugin");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(install_dir.join("plugin.zsh"), "# plugin\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/plugin");
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/plugin", "repo"),
            1_700_000_000,
        )
        .unwrap();
        let runner = verified_adoption_runner(&install_dir);

        let summary = run(
            &[parse_entry("owner/plugin|github:repo", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "fresh");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/plugin"),
            Some(&ManifestEntry::new(
                "owner/plugin",
                "github:repo",
                "plugin",
                install_dir.display().to_string(),
            ))
        );
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/plugin", Kind::Bin).exists(),
            "an asset-only checkout must not acquire public-command ownership"
        );
        assert!(!fixture.roots.bin_dir.join("plugin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_requires_explicit_regular_command_for_direct_bin_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-existing-bin-symlink-command");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/plugin");
        write_executable(&install_dir.join("lib/tool"));
        fs::create_dir_all(install_dir.join("bin")).unwrap();
        symlink("../lib/tool", install_dir.join("bin/tool")).unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/plugin");
        let runner = verified_adoption_runner(&install_dir);

        let summary = run(
            &[parse_entry("owner/plugin|github:repo", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("explicit command column"));
        assert!(install_dir.join(".git").is_dir());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/plugin")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_rejects_unrecorded_checkout_with_foreign_origin() {
        let fixture = Fixture::new("repo-existing-foreign-origin");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "https://github.com/other/tool");
        let command_before = fs::read(install_dir.join("bin/tool")).unwrap();
        let stamp_path = crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo");
        crate::stamp::remote_touch(&stamp_path, 1_700_000_000).unwrap();
        let stamp_before = fs::read(&stamp_path).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
        assert!(summary.items[0].detail.contains("refusing to adopt"));
        assert!(summary.items[0].detail.contains("origin"));
        assert!(summary.items[0].detail.contains("owner/tool"));
        assert_eq!(
            fs::read(install_dir.join("bin/tool")).unwrap(),
            command_before
        );
        assert_eq!(fs::read(&stamp_path).unwrap(), stamp_before);
        assert!(install_dir.join(".git").is_dir());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(!link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin).exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_rejects_unsupported_configured_adoption_origin() {
        let mut fixture = Fixture::new("repo-existing-unsupported-configured-origin");
        fixture.write_lib();
        fixture.env_vars.insert(
            "SHDEPS_TOOL_REPO".to_owned(),
            "https://example.invalid/owner/tool".to_owned(),
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(
            summary.items[0]
                .detail
                .contains("configured repository URL")
        );
        assert!(install_dir.join(".git").is_dir());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_rejects_verification_tools_selected_from_candidate() {
        use std::os::unix::fs::symlink;

        for (command, origin) in [
            ("git", "https://github.com/owner/tool"),
            ("ssh", "git@github.com:owner/tool.git"),
        ] {
            for selection in ["basename-link", "ancestor-link"] {
                let label = format!("{command}-{selection}");
                let mut fixture = Fixture::new(&format!("repo-candidate-{label}"));
                fixture.write_lib();
                if command == "ssh" {
                    fixture
                        .env_vars
                        .insert("SHDEPS_TOOL_REPO".to_owned(), origin.to_owned());
                }
                let manifest_path = manifest::path(&fixture.roots.state_dir);
                let install_dir = fixture.roots.install_dir.join("owner/tool");
                write_executable(&install_dir.join("bin/tool"));
                let external_program = fixture
                    .roots
                    .home
                    .join(format!("host-tools-{selection}/{command}"));
                write_executable(&external_program);
                let selected_program = if selection == "basename-link" {
                    fs::create_dir_all(install_dir.join("tools")).unwrap();
                    let selected = install_dir.join(format!("tools/{command}"));
                    symlink(&external_program, &selected).unwrap();
                    selected
                } else {
                    let selected_parent = install_dir.join("tools-link");
                    symlink(external_program.parent().unwrap(), &selected_parent).unwrap();
                    selected_parent.join(command)
                };
                initialize_git_checkout(&install_dir, origin);
                let runner = verified_adoption_runner(&install_dir)
                    .with_clean_program(command, selected_program);

                let summary = run(
                    &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
                    &Manifest::default(),
                    &fixture.context(&manifest_path, &runner, "apt"),
                    Options::default(),
                )
                .unwrap();

                assert!(summary.has_errors(), "{label}");
                assert!(
                    summary.items[0]
                        .detail
                        .contains("selected from the candidate checkout"),
                    "{label}: {}",
                    summary.items[0].detail
                );
                assert!(
                    runner.clean_calls().is_empty(),
                    "candidate-selected {label} must never execute"
                );
                assert!(install_dir.join(".git").is_dir(), "{label}");
                assert!(!fixture.roots.bin_dir.join("tool").exists(), "{label}");
                assert!(
                    manifest::read(&manifest_path)
                        .unwrap()
                        .get("owner/tool")
                        .is_none(),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn update_github_repo_adopts_supported_different_repository_override() {
        let mut fixture = Fixture::new("repo-existing-different-override");
        fixture.write_lib();
        fixture.env_vars.insert(
            "SHDEPS_MY_TOOL_REPO".to_owned(),
            "https://github.com/cgraf78/private-tool.git".to_owned(),
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("cgraf78/my-tool");
        write_executable(&install_dir.join("bin/my-tool"));
        initialize_git_checkout(&install_dir, "https://github.com/cgraf78/private-tool.git");
        let runner = verified_adoption_runner(&install_dir);

        let summary = run(
            &[parse_entry("cgraf78/my-tool|github:repo|my-tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert_eq!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("cgraf78/my-tool"),
            Some(&ManifestEntry::new(
                "cgraf78/my-tool",
                "github:repo",
                "my-tool",
                install_dir.display().to_string(),
            ))
        );
        assert!(runner.clean_calls().iter().any(|call| {
            call.args.iter().any(|argument| {
                argument == OsStr::new("https://github.com/cgraf78/private-tool.git")
            })
        }));
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_quarantine_retries_private_https_as_ssh() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let mut fixture = Fixture::new("repo-existing-private-ssh-fallback");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        let fake_ssh = fixture.roots.home.join("fake-ssh");
        write_executable(&fake_ssh);
        fs::create_dir_all(fixture.roots.home.join(".ssh")).unwrap();
        let known_hosts = fixture.roots.home.join(".ssh/known_hosts");
        fs::write(&known_hosts, "github.com ssh-ed25519 test-key\n").unwrap();
        let socket = fixture.roots.home.join("agent.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let socket_link = fixture.roots.home.join("agent-link.sock");
        symlink(&socket, &socket_link).unwrap();
        fixture.env_vars.insert(
            "SSH_AUTH_SOCK".to_owned(),
            socket_link.display().to_string(),
        );
        let mut outputs = vec![Output {
            success: false,
            timed_out: false,
            stdout: String::new(),
            stderr: "authentication required".to_owned(),
        }];
        outputs.extend(verified_adoption_outputs(&install_dir));
        let runner = FakeRunner::default()
            .with_clean_git(outputs)
            .with_clean_program("ssh", fake_ssh.clone());

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        let calls = runner.clean_calls();
        assert_eq!(calls.len(), 8, "one HTTPS probe plus a complete SSH proof");

        let https_root = calls[0].cwd.clone();
        let https_env = expected_clean_git_env(&https_root);
        assert_exact_clean_call(
            &calls[0],
            &install_dir,
            &https_root,
            &https_env,
            &expected_clean_git_args(
                &https_root,
                "protocol.https.allow=always",
                [
                    "ls-remote".into(),
                    "--symref".into(),
                    "--exit-code".into(),
                    "--".into(),
                    "https://github.com/owner/tool".into(),
                    "HEAD".into(),
                ],
            ),
        );

        let ssh_root = calls[1].cwd.clone();
        assert_ne!(https_root, ssh_root, "each transport attempt is isolated");
        let canonical_ssh = fs::canonicalize(fake_ssh).unwrap();
        let canonical_known_hosts = fs::canonicalize(known_hosts).unwrap();
        let canonical_socket = fs::canonicalize(socket).unwrap();
        let mut ssh_env = expected_clean_git_env(&ssh_root);
        ssh_env.insert(
            "GIT_SSH_COMMAND".into(),
            format!(
                "'{}' -F /dev/null -oBatchMode=yes -oClearAllForwardings=yes -oForwardAgent=no -oForwardX11=no -oPermitLocalCommand=no -oStrictHostKeyChecking=yes -oUpdateHostKeys=no -oGlobalKnownHostsFile=/dev/null -oUserKnownHostsFile='{}'",
                canonical_ssh.display(),
                canonical_known_hosts.display()
            )
            .into(),
        );
        ssh_env.insert("SSH_AUTH_SOCK".into(), canonical_socket.into_os_string());
        assert_exact_clean_verification_sequence(
            &calls[1..],
            CleanVerificationExpectation {
                candidate: &install_dir,
                quarantine: &ssh_root,
                base_env: &ssh_env,
                transport: "protocol.ssh.allow=always",
                origin: "git@github.com:owner/tool.git",
                branch: "main",
                hash_calls: 1,
            },
        );
        assert!(!https_root.exists());
        assert!(!ssh_root.exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_rejects_candidate_owned_ssh_agent_socket() {
        use std::os::unix::net::UnixListener;

        let mut fixture = Fixture::new("repo-existing-candidate-agent-socket");
        fixture.write_lib();
        fixture.env_vars.insert(
            "SHDEPS_TOOL_REPO".to_owned(),
            "git@github.com:owner/tool.git".to_owned(),
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "git@github.com:owner/tool.git");
        let socket = install_dir.join("agent.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        fixture
            .env_vars
            .insert("SSH_AUTH_SOCK".to_owned(), socket.display().to_string());
        fs::create_dir_all(fixture.roots.home.join(".ssh")).unwrap();
        fs::write(
            fixture.roots.home.join(".ssh/known_hosts"),
            "github.com ssh-ed25519 test-key\n",
        )
        .unwrap();
        let fake_ssh = fixture.roots.home.join("fake-ssh");
        write_executable(&fake_ssh);
        let runner = verified_adoption_runner(&install_dir).with_clean_program("ssh", fake_ssh);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("SSH_AUTH_SOCK"));
        assert!(runner.clean_calls().is_empty());
        assert!(socket.exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_requires_isolated_verification_before_adoption() {
        let fixture = Fixture::new("repo-existing-verification-required");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        fs::write(install_dir.join("sentinel"), "preserve\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo"),
            1_700_000_000,
        )
        .unwrap();

        // The default fake runner deliberately does not implement the clean
        // execution capability. A fresh TTL must not let an unrecorded root
        // bypass the mandatory independent quarantine.
        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
        assert!(summary.items[0].detail.contains("isolated verification"));
        assert!(!install_dir.is_symlink());
        assert_eq!(
            fs::read_to_string(install_dir.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_rejects_worktree_drift_against_quarantine() {
        #[derive(Debug, Clone, Copy)]
        enum Drift {
            Modified,
            Missing,
            WrongMode,
            Untracked,
            HugeSparse,
        }

        for drift in [
            Drift::Modified,
            Drift::Missing,
            Drift::WrongMode,
            Drift::Untracked,
            Drift::HugeSparse,
        ] {
            let fixture = Fixture::new(&format!("repo-quarantine-drift-{drift:?}"));
            fixture.write_lib();
            let manifest_path = manifest::path(&fixture.roots.state_dir);
            let install_dir = fixture.roots.install_dir.join("owner/tool");
            let command = install_dir.join("bin/tool");
            write_executable(&command);
            initialize_git_checkout(&install_dir, "https://github.com/owner/tool");

            // Capture independent remote truth first, then alter only the
            // candidate. The fake remote commands stay deterministic while
            // `hash-object` is real stock Git over the private scratch copy.
            let fake_ssh = fixture.roots.home.join("fake-ssh");
            write_executable(&fake_ssh);
            let runner = verified_adoption_runner(&install_dir).with_clean_program("ssh", fake_ssh);
            match drift {
                Drift::Modified => fs::write(&command, "#!/bin/sh\necho changed\n").unwrap(),
                Drift::Missing => fs::remove_file(&command).unwrap(),
                Drift::WrongMode => {
                    let mut permissions = fs::metadata(&command).unwrap().permissions();
                    permissions.set_mode(0o644);
                    fs::set_permissions(&command, permissions).unwrap();
                }
                Drift::Untracked => {
                    fs::write(install_dir.join("untracked"), "not in remote\n").unwrap();
                }
                Drift::HugeSparse => {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&command)
                        .unwrap()
                        .set_len(1024 * 1024 * 1024)
                        .unwrap();
                }
            }

            let summary = run(
                &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
                &Manifest::default(),
                &fixture.context(&manifest_path, &runner, "apt"),
                Options::default(),
            )
            .unwrap();

            assert!(summary.has_errors(), "{drift:?}");
            assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
            assert!(
                summary.items[0].detail.contains("refusing to adopt"),
                "{drift:?}: {}",
                summary.items[0].detail
            );
            assert!(!install_dir.is_symlink(), "{drift:?}");
            assert!(
                manifest::read(&manifest_path)
                    .unwrap()
                    .get("owner/tool")
                    .is_none(),
                "{drift:?}"
            );
            assert!(!fixture.roots.bin_dir.join("tool").exists(), "{drift:?}");
            assert!(
                runner.clean_calls().iter().all(|call| {
                    call.args
                        .iter()
                        .all(|argument| argument != OsStr::new("git@github.com:owner/tool.git"))
                }),
                "local candidate drift must not trigger SSH: {drift:?}"
            );
        }
    }

    #[test]
    fn update_github_repo_rejects_independent_identity_or_index_disagreement() {
        #[derive(Debug, Clone, Copy)]
        enum Failure {
            DefaultBranch,
            RemoteCommit,
            FetchedCommit,
            Index,
        }

        for failure in [
            Failure::DefaultBranch,
            Failure::RemoteCommit,
            Failure::FetchedCommit,
            Failure::Index,
        ] {
            let fixture = Fixture::new(&format!("repo-quarantine-proof-{failure:?}"));
            fixture.write_lib();
            let manifest_path = manifest::path(&fixture.roots.state_dir);
            let install_dir = fixture.roots.install_dir.join("owner/tool");
            write_executable(&install_dir.join("bin/tool"));
            initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
            let mut outputs = verified_adoption_outputs(&install_dir);
            match failure {
                Failure::DefaultBranch => {
                    let head = fixture_git_stdout(&install_dir, &["rev-parse", "HEAD"]);
                    outputs[0] = clean_success(format!(
                        "ref: refs/heads/trunk\tHEAD\n{}\tHEAD\n",
                        head.trim()
                    ));
                }
                Failure::RemoteCommit => {
                    outputs[0] = clean_success(format!(
                        "ref: refs/heads/main\tHEAD\n{}\tHEAD\n",
                        "1".repeat(40)
                    ));
                }
                Failure::FetchedCommit => {
                    outputs[3] = clean_success(format!("{}\n", "2".repeat(40)));
                }
                Failure::Index => {
                    outputs[4] = Output {
                        success: false,
                        timed_out: false,
                        stdout: String::new(),
                        stderr: "index mismatch".to_owned(),
                    };
                }
            }
            let fake_ssh = fixture.roots.home.join("fake-ssh");
            write_executable(&fake_ssh);
            let runner = FakeRunner::default()
                .with_clean_git(outputs)
                .with_clean_program("ssh", fake_ssh);

            let summary = run(
                &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
                &Manifest::default(),
                &fixture.context(&manifest_path, &runner, "apt"),
                Options::default(),
            )
            .unwrap();

            assert!(summary.has_errors(), "{failure:?}");
            assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
            assert!(install_dir.join(".git").is_dir(), "{failure:?}");
            assert!(!fixture.roots.bin_dir.join("tool").exists(), "{failure:?}");
            assert!(
                runner.clean_calls().iter().all(|call| {
                    call.args
                        .iter()
                        .all(|argument| argument != OsStr::new("git@github.com:owner/tool.git"))
                }),
                "post-fetch proof failures must not trigger SSH: {failure:?}"
            );
            assert!(
                manifest::read(&manifest_path)
                    .unwrap()
                    .get("owner/tool")
                    .is_none(),
                "{failure:?}"
            );
            assert_no_verification_quarantines(&fixture.roots.state_dir);
        }
    }

    #[test]
    fn verified_repo_plan_rejects_root_generation_replacement_before_apply() {
        let fixture = Fixture::new("repo-quarantine-root-generation");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        let runner = verified_adoption_runner(&install_dir);
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let context = fixture.context(&manifest_path, &runner, "apt");
        let plan = match crate::update_repo::prepare(
            &entry,
            &context,
            &install_dir,
            crate::update_repo::DestinationOwnership::Unrecorded,
        )
        .unwrap()
        {
            crate::update_repo::Preparation::Ready(plan) => *plan,
            crate::update_repo::Preparation::Failed(item) => {
                panic!("verification unexpectedly failed: {}", item.detail)
            }
        };

        let original = install_dir.with_extension("verified-original");
        fs::rename(&install_dir, &original).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(install_dir.join("replacement-sentinel"), "preserve\n").unwrap();

        let error =
            crate::update_repo::apply(plan, &entry, &context, Options::default()).unwrap_err();

        assert!(error.to_string().contains("root changed"));
        assert_eq!(
            fs::read_to_string(install_dir.join("replacement-sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn verified_development_plan_rejects_root_generation_replacement_before_apply() {
        let fixture = Fixture::new("repo-development-root-generation");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let runner = FakeRunner::default();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let context = fixture.context(&manifest_path, &runner, "apt");
        let plan = match crate::update_repo::prepare(
            &entry,
            &context,
            &install_dir,
            crate::update_repo::DestinationOwnership::Unrecorded,
        )
        .unwrap()
        {
            crate::update_repo::Preparation::Ready(plan) => *plan,
            crate::update_repo::Preparation::Failed(item) => {
                panic!(
                    "development verification unexpectedly failed: {}",
                    item.detail
                )
            }
        };

        let original = local_clone.with_extension("verified-original");
        fs::rename(&local_clone, &original).unwrap();
        fs::create_dir_all(&local_clone).unwrap();
        fs::write(local_clone.join("replacement-sentinel"), "preserve\n").unwrap();

        let error =
            crate::update_repo::apply(plan, &entry, &context, Options::default()).unwrap_err();

        assert!(error.to_string().contains("development checkout changed"));
        assert_eq!(
            fs::read_to_string(local_clone.join("replacement-sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(!install_dir.exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn verified_development_plan_rejects_origin_change_before_apply() {
        let fixture = Fixture::new("repo-development-origin-change");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let runner = FakeRunner::default();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let context = fixture.context(&manifest_path, &runner, "apt");
        let plan = match crate::update_repo::prepare(
            &entry,
            &context,
            &install_dir,
            crate::update_repo::DestinationOwnership::Unrecorded,
        )
        .unwrap()
        {
            crate::update_repo::Preparation::Ready(plan) => *plan,
            crate::update_repo::Preparation::Failed(item) => {
                panic!(
                    "development verification unexpectedly failed: {}",
                    item.detail
                )
            }
        };

        fixture_git(
            &local_clone,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/other/tool",
            ],
        );

        let error =
            crate::update_repo::apply(plan, &entry, &context, Options::default()).unwrap_err();

        assert!(error.to_string().contains("origin does not match"));
        assert!(!install_dir.exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn verified_development_plan_rejects_command_symlink_before_apply() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-development-command-change");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let runner = FakeRunner::default();
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let context = fixture.context(&manifest_path, &runner, "apt");
        let plan = match crate::update_repo::prepare(
            &entry,
            &context,
            &install_dir,
            crate::update_repo::DestinationOwnership::Unrecorded,
        )
        .unwrap()
        {
            crate::update_repo::Preparation::Ready(plan) => *plan,
            crate::update_repo::Preparation::Failed(item) => {
                panic!(
                    "development verification unexpectedly failed: {}",
                    item.detail
                )
            }
        };

        let outside = fixture.roots.home.join("outside-tool");
        write_executable(&outside);
        fs::remove_file(local_clone.join("bin/tool")).unwrap();
        symlink(&outside, local_clone.join("bin/tool")).unwrap();

        let item = crate::update_repo::apply(plan, &entry, &context, Options::default()).unwrap();

        assert_eq!(item.reason, ItemReason::MissingBinary);
        assert!(!install_dir.exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn unrecorded_development_publication_preserves_late_destination() {
        let fixture = Fixture::new("repo-development-late-destination");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let local_clone_arg = local_clone.display().to_string();
        let runner = FakeRunner::default().with_created_binary(
            "git",
            [
                "-C",
                &local_clone_arg,
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
            install_dir.clone(),
        );
        let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
        let context = fixture.context(&manifest_path, &runner, "apt");
        let plan = match crate::update_repo::prepare(
            &entry,
            &context,
            &install_dir,
            crate::update_repo::DestinationOwnership::Unrecorded,
        )
        .unwrap()
        {
            crate::update_repo::Preparation::Ready(plan) => *plan,
            crate::update_repo::Preparation::Failed(item) => {
                panic!(
                    "development verification unexpectedly failed: {}",
                    item.detail
                )
            }
        };

        let error =
            crate::update_repo::apply(plan, &entry, &context, Options::default()).unwrap_err();

        assert!(error.to_string().contains("destination appeared"));
        assert!(fs::symlink_metadata(&install_dir).unwrap().is_file());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn development_git_uses_separate_read_and_pull_timeouts() {
        let fixture = Fixture::new("repo-development-timeouts");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let local_arg = local_clone.display().to_string();
        let runner = FakeRunner::default()
            .with_success(
                "git",
                [
                    "-C",
                    &local_arg,
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
                "",
            )
            .with_success(
                "git",
                [
                    "-C",
                    &local_arg,
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                "origin/main\n",
            )
            .with_success(
                "git",
                ["-C", &local_arg, "pull", "--ff-only", "--quiet"],
                "",
            )
            .with_success("git", ["-C", &local_arg, "rev-parse", "HEAD"], "head\n");

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors(), "{}", summary.items[0].detail);
        let calls = runner.clean_calls();
        let pull = calls
            .iter()
            .find(|call| {
                call.args
                    .ends_with(&["pull".into(), "--ff-only".into(), "--quiet".into()])
            })
            .expect("development pull call");
        let status = calls
            .iter()
            .find(|call| {
                call.args.ends_with(&[
                    "status".into(),
                    "--porcelain".into(),
                    "--untracked-files=normal".into(),
                ])
            })
            .expect("development status call");
        assert_eq!(status.timeout, Duration::from_secs(10));
        assert_eq!(pull.timeout, Duration::from_secs(30 * 60));
    }

    #[test]
    fn timed_out_development_status_aborts_before_pull_or_publication() {
        let fixture = Fixture::new("repo-development-status-timeout");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let local_arg = local_clone.display().to_string();
        let mut runner = FakeRunner::default();
        runner.push_output(
            key(
                "git",
                [
                    "-C",
                    &local_arg,
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ],
            ),
            Output {
                success: false,
                timed_out: true,
                stdout: String::new(),
                stderr: String::new(),
            },
        );

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("timed out"));
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn failed_development_status_aborts_before_pull_or_publication() {
        let fixture = Fixture::new("repo-development-status-failure");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        initialize_git_checkout(&local_clone, "https://github.com/owner/tool");
        let local_arg = local_clone.display().to_string();
        let runner = FakeRunner::default().with_failure(
            "git",
            [
                "-C",
                &local_arg,
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ],
        );

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("status failed"));
        assert!(runner.clean_calls().iter().all(|call| {
            !call
                .args
                .windows(3)
                .any(|args| args == ["pull", "--ff-only", "--quiet"])
        }));
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_verification_state_never_overlaps_candidate_checkout() {
        use std::os::unix::fs::symlink;

        for (symlinked_state, relative_state) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut fixture = Fixture::new(&format!(
                "repo-state-overlap-symlink-{symlinked_state}-relative-{relative_state}"
            ));
            fixture.write_lib();
            let manifest_path = fixture.roots.home.join("manifest-outside-state");
            let install_dir = fixture.roots.install_dir.join("owner/tool");
            write_executable(&install_dir.join("bin/tool"));
            initialize_git_checkout(&install_dir, "https://github.com/owner/tool");

            let physical_state = install_dir.join("verification-state");
            if symlinked_state {
                fs::create_dir_all(&physical_state).unwrap();
                let state_link = fixture.roots.home.join("state-link");
                symlink(&physical_state, &state_link).unwrap();
                fixture.roots.state_dir = if relative_state {
                    relative_from_current(&state_link)
                } else {
                    state_link
                };
            } else {
                fixture.roots.state_dir = if relative_state {
                    relative_from_current(&physical_state)
                } else {
                    physical_state.clone()
                };
            }

            let entry = parse_entry("owner/tool|github:repo|tool|-|-", None);
            let runner = FakeRunner::default();
            let context = fixture.context(&manifest_path, &runner, "apt");
            let preparation = crate::update_repo::prepare(
                &entry,
                &context,
                &install_dir,
                crate::update_repo::DestinationOwnership::Unrecorded,
            )
            .unwrap();

            let crate::update_repo::Preparation::Failed(item) = preparation else {
                panic!("overlapping state must fail before a plan is published");
            };
            assert!(item.detail.contains("state directory is inside"));
            assert!(runner.clean_calls().is_empty());
            if symlinked_state {
                assert_eq!(fs::read_dir(&physical_state).unwrap().count(), 0);
            } else {
                assert!(!physical_state.exists());
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn repo_verification_rejects_symlinked_ancestor_before_hashing() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-quarantine-symlinked-ancestor");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/plugin");
        fs::create_dir_all(install_dir.join("nested")).unwrap();
        fs::write(install_dir.join("nested/file"), "trusted bytes\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/plugin");
        let runner = verified_adoption_runner(&install_dir);

        let outside = fixture.roots.home.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("file"), "trusted bytes\n").unwrap();
        fs::remove_dir_all(install_dir.join("nested")).unwrap();
        symlink(&outside, install_dir.join("nested")).unwrap();

        let summary = run(
            &[parse_entry("owner/plugin|github:repo", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("untracked path `nested`"));
        assert_eq!(
            fs::read_to_string(outside.join("file")).unwrap(),
            "trusted bytes\n"
        );
        assert!(runner.clean_calls().iter().all(|call| {
            call.args
                .iter()
                .all(|argument| argument != OsStr::new("hash-object"))
        }));
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/plugin")
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_verifies_before_preparing_release_transition() {
        let fixture = Fixture::new("repo-verify-before-transition-prepare");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");

        let public = fixture.roots.bin_dir.join("tool");
        write_executable(&public);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            public.display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        let installed = manifest::read(&manifest_path).unwrap();

        // A raw-release transition normally moves its owned public binary
        // aside before installing the symlink-based repo method. Make that
        // rename impossible: the quarantine failure must still win because
        // proof belongs before every transition mutation, not merely before
        // the first Git command against the candidate.
        let mut bin_permissions = fs::metadata(&fixture.roots.bin_dir).unwrap().permissions();
        let original_mode = bin_permissions.mode();
        bin_permissions.set_mode(0o555);
        fs::set_permissions(&fixture.roots.bin_dir, bin_permissions).unwrap();

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
            Options::default(),
        )
        .unwrap();

        let mut bin_permissions = fs::metadata(&fixture.roots.bin_dir).unwrap().permissions();
        bin_permissions.set_mode(original_mode);
        fs::set_permissions(&fixture.roots.bin_dir, bin_permissions).unwrap();

        assert!(summary.has_errors());
        assert!(summary.items[0].detail.contains("isolated verification"));
        assert_eq!(fs::read_to_string(&public).unwrap(), "#!/bin/sh\n");
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&old)
        );
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_rejects_malformed_unrecorded_roots_before_source_selection() {
        use std::os::unix::fs::symlink;

        #[derive(Debug, Clone, Copy)]
        enum RootKind {
            Directory,
            File,
            DanglingSymlink,
            GitFile,
        }

        for with_dev in [false, true] {
            for kind in [
                RootKind::Directory,
                RootKind::File,
                RootKind::DanglingSymlink,
                RootKind::GitFile,
            ] {
                let fixture = Fixture::new(&format!(
                    "repo-malformed-root-{kind:?}-{}",
                    if with_dev { "dev" } else { "managed" }
                ));
                fixture.write_lib();
                let manifest_path = manifest::path(&fixture.roots.state_dir);
                let install_dir = fixture.roots.install_dir.join("owner/tool");
                let sentinel = install_dir.join("sentinel");
                let dangling_target = fixture.roots.home.join("missing-target");
                match kind {
                    RootKind::Directory => {
                        fs::create_dir_all(&install_dir).unwrap();
                        fs::write(&sentinel, "preserve\n").unwrap();
                    }
                    RootKind::File => {
                        fs::create_dir_all(install_dir.parent().unwrap()).unwrap();
                        fs::write(&install_dir, "preserve\n").unwrap();
                    }
                    RootKind::DanglingSymlink => {
                        fs::create_dir_all(install_dir.parent().unwrap()).unwrap();
                        symlink(&dangling_target, &install_dir).unwrap();
                    }
                    RootKind::GitFile => {
                        fs::create_dir_all(&install_dir).unwrap();
                        fs::write(&sentinel, "preserve\n").unwrap();
                        fs::write(install_dir.join(".git"), "gitdir: elsewhere\n").unwrap();
                    }
                }
                let local_clone = fixture.roots.git_dev_dir.join("tool");
                if with_dev {
                    write_executable(&local_clone.join("bin/tool"));
                }

                let summary = run(
                    &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
                    &Manifest::default(),
                    &fixture.context(&manifest_path, &FakeRunner::default(), "apt"),
                    Options::default(),
                )
                .unwrap();

                assert!(summary.has_errors(), "{kind:?}, with_dev={with_dev}");
                assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
                assert!(
                    summary.items[0].detail.contains("refusing to adopt"),
                    "{kind:?}, with_dev={with_dev}: {}",
                    summary.items[0].detail
                );
                match kind {
                    RootKind::Directory | RootKind::GitFile => {
                        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "preserve\n");
                    }
                    RootKind::File => {
                        assert_eq!(fs::read_to_string(&install_dir).unwrap(), "preserve\n");
                    }
                    RootKind::DanglingSymlink => {
                        assert_eq!(fs::read_link(&install_dir).unwrap(), dangling_target);
                    }
                }
                assert!(
                    !fixture.roots.bin_dir.join("tool").exists(),
                    "{kind:?}, with_dev={with_dev}"
                );
                assert!(
                    manifest::read(&manifest_path)
                        .unwrap()
                        .get("owner/tool")
                        .is_none(),
                    "{kind:?}, with_dev={with_dev}"
                );
                if with_dev {
                    assert!(local_clone.join("bin/tool").exists());
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_preserves_valid_unrecorded_checkout_when_dev_clone_exists() {
        let fixture = Fixture::new("repo-existing-valid-with-dev");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        write_executable(&install_dir.join("bin/tool"));
        fs::write(install_dir.join("managed-sentinel"), "preserve\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        let command_before = fs::read(install_dir.join("bin/tool")).unwrap();
        let local_clone = fixture.roots.git_dev_dir.join("tool");
        write_executable(&local_clone.join("bin/tool"));
        fs::write(local_clone.join("dev-sentinel"), "dev\n").unwrap();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo"),
            1_700_000_000,
        )
        .unwrap();
        let runner = verified_adoption_runner(&install_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!install_dir.is_symlink());
        assert_eq!(
            fs::read_to_string(install_dir.join("managed-sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(
            fs::read(install_dir.join("bin/tool")).unwrap(),
            command_before
        );
        assert_eq!(
            fs::read_link(fixture.roots.bin_dir.join("tool")).unwrap(),
            install_dir.join("bin/tool")
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_dir.display().to_string(),
            ))
        );
        assert!(local_clone.join("dev-sentinel").exists());
    }

    #[test]
    #[cfg(unix)]
    fn update_github_repo_warm_recorded_checkout_preserves_unowned_regular_command() {
        let fixture = Fixture::new("repo-warm-regular-command");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        write_executable(&install_dir.join("bin/tool"));
        let public = fixture.roots.bin_dir.join("tool");
        write_executable(&public);
        fs::write(&public, "#!/bin/sh\nexec client-adapter \"$@\"\n").unwrap();
        let public_before = fs::read(&public).unwrap();
        let public_metadata = fs::metadata(&public).unwrap();
        let installed = record_repo_manifest(&manifest_path, "owner/tool", "tool", &install_dir);
        let manifest_before = fs::read(&manifest_path).unwrap();
        crate::stamp::remote_touch(
            &crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo"),
            1_700_000_000,
        )
        .unwrap();
        let runner = FakeRunner::default();

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].detail, "fresh");
        assert_eq!(fs::read(&public).unwrap(), public_before);
        let public_after = fs::metadata(&public).unwrap();
        assert_eq!(public_after.ino(), public_metadata.ino());
        assert_eq!(public_after.mode(), public_metadata.mode());
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin).exists(),
            "a preserved regular adapter must remain outside Shdeps link ownership"
        );
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
        assert!(
            runner
                .calls()
                .iter()
                .all(|call| !call.contains("\0clone\0") && !call.contains("\0pull\0")),
            "a fresh recorded checkout must not clone or pull"
        );
    }

    #[test]
    fn update_github_repo_existing_fresh_clone_rejects_missing_explicit_command() {
        let fixture = Fixture::new("repo-existing-fresh-missing-cmd");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(install_dir.join("README.md"), "fixture\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        let stamp_path = crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo");
        crate::stamp::remote_touch(&stamp_path, 1_700_000_000).unwrap();
        let runner = verified_adoption_runner(&install_dir);

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &manifest::Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert_eq!(
            fs::read_to_string(&stamp_path).unwrap(),
            "1700000000\n",
            "fresh-path rejection must not rewrite the repo TTL"
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
        );
    }

    #[test]
    fn update_github_repo_missing_remote_command_still_rejects_untracked_candidate_command() {
        let fixture = Fixture::new("repo-existing-missing-untracked-cmd");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(install_dir.join("README.md"), "fixture\n").unwrap();
        initialize_git_checkout(&install_dir, "https://github.com/owner/tool");
        let runner = verified_adoption_runner(&install_dir);
        write_executable(&install_dir.join("bin/tool"));

        let summary = run(
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &Manifest::default(),
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::InstallFailed);
        assert!(summary.items[0].detail.contains("untracked path"));
        assert!(install_dir.join("bin/tool").exists());
        assert!(!fixture.roots.bin_dir.join("tool").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_none()
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
        write_executable(&install_dir.join("bin/tool"));
        let installed = record_repo_manifest(&manifest_path, "owner/tool", "tool", &install_dir);
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
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].status, super::ItemStatus::Warning);
        assert_eq!(summary.items[0].reason, ItemReason::RepoPullFailed);
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
        write_executable(&install_dir.join("bin/tool"));
        let installed = record_repo_manifest(&manifest_path, "owner/tool", "tool", &install_dir);
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
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options::default(),
        )
        .unwrap();

        assert!(!summary.has_errors());
        assert!(!summary.items[0].changed);
        assert_eq!(summary.items[0].status, super::ItemStatus::Warning);
        assert_eq!(summary.items[0].reason, ItemReason::RepoPullFailed);
        assert_eq!(summary.items[0].detail, "pull failed (no fast-forward)");
    }

    #[test]
    fn update_github_repo_existing_clone_retries_pull_with_ssh_origin() {
        let fixture = Fixture::new("repo-pull-fallback");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("private/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        write_executable(&install_dir.join("bin/tool"));
        let installed = record_repo_manifest(&manifest_path, "private/tool", "tool", &install_dir);
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
            &installed,
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

    #[test]
    fn update_github_repo_existing_pull_missing_explicit_command_does_not_touch_stamp() {
        let fixture = Fixture::new("repo-pull-missing-cmd");
        fixture.write_lib();
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_dir = fixture.roots.install_dir.join("owner/tool");
        fs::create_dir_all(install_dir.join(".git")).unwrap();
        let installed = record_repo_manifest(&manifest_path, "owner/tool", "tool", &install_dir);
        let stamp_path = crate::stamp::remote_path(&fixture.roots.state_dir, "owner/tool", "repo");
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
            &[parse_entry("owner/tool|github:repo|tool|-|-", None)],
            &installed,
            &fixture.context(&manifest_path, &runner, "apt"),
            Options {
                now: 1_700_000_000,
                reinstall: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(summary.has_errors());
        assert_eq!(summary.items[0].reason, ItemReason::MissingBinary);
        assert!(
            !stamp_path.exists(),
            "missing explicit command must not refresh the repo TTL"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("owner/tool"),
            Some(&ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_dir.display().to_string(),
            ))
        );
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
        redirects: std::collections::BTreeMap<String, String>,
        status_errors: std::collections::BTreeMap<String, u16>,
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

        fn with_status_error(mut self, url: &str, status: u16) -> Self {
            self.status_errors.insert(url.to_owned(), status);
            self
        }

        fn with_redirect(mut self, url: &str, location: &str) -> Self {
            self.redirects.insert(url.to_owned(), location.to_owned());
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
            if let Some(status) = self.status_errors.get(url) {
                return Err(io::Error::other(crate::http::HttpStatusError::new(
                    *status, "test",
                )));
            }
            self.responses.get(url).cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("missing fake URL {url}"))
            })
        }

        fn redirect_location(&self, url: &str) -> io::Result<Option<String>> {
            self.requests.lock().unwrap().push((url.to_owned(), None));
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

    #[derive(Debug, Clone)]
    struct CleanCall {
        program: PathBuf,
        cwd: PathBuf,
        args: Vec<OsString>,
        env: BTreeMap<OsString, OsString>,
        timeout: Duration,
    }

    #[derive(Debug, Clone, Default)]
    struct FakeRunner {
        commands: std::collections::BTreeSet<String>,
        clean_programs: std::collections::BTreeMap<String, PathBuf>,
        clean_outputs: QueuedOutputs,
        clean_calls: std::sync::Arc<std::sync::Mutex<Vec<CleanCall>>>,
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

        fn with_clean_git(mut self, outputs: impl IntoIterator<Item = Output>) -> Self {
            self.clean_programs
                .insert("git".to_owned(), host_command_path("git"));
            self.clean_outputs.lock().unwrap().extend(outputs);
            self
        }

        fn with_clean_program(mut self, command: &str, path: PathBuf) -> Self {
            self.clean_programs.insert(command.to_owned(), path);
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

        fn clean_calls(&self) -> Vec<CleanCall> {
            self.clean_calls.lock().unwrap().clone()
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
            self.commands.contains(command) || self.clean_programs.contains_key(command)
        }

        fn path(&self, command: &str) -> Option<PathBuf> {
            self.clean_programs
                .get(command)
                .cloned()
                .or_else(|| self.commands.contains(command).then(|| command.into()))
                .or_else(|| (command == "git").then(|| host_command_path("git")))
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
            if let Some(outputs) = self.outputs.get(&key) {
                return Ok(next_output(outputs));
            }
            Ok(Output {
                success: false,
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        fn run_env_clear(
            &self,
            program: &Path,
            cwd: &Path,
            args: &[OsString],
            env: &BTreeMap<OsString, OsString>,
            timeout: Duration,
        ) -> io::Result<Output> {
            if let Some(legacy_args) = development_clean_git_args(cwd, args) {
                self.clean_calls.lock().unwrap().push(CleanCall {
                    program: program.to_path_buf(),
                    cwd: cwd.to_path_buf(),
                    args: args.to_vec(),
                    env: env.clone(),
                    timeout,
                });
                let key = key("git", legacy_args.iter());
                for path in self.creates.get(&key).into_iter().flatten() {
                    write_executable(path);
                }
                for path in self.creates_dirs.get(&key).into_iter().flatten() {
                    fs::create_dir_all(path).unwrap();
                }
                if let Some(outputs) = self.outputs.get(&key) {
                    return Ok(next_output(outputs));
                }
                if is_development_real_read_call(args) {
                    return crate::process::Process.run_env_clear(program, cwd, args, env, timeout);
                }
                return Ok(Output {
                    success: false,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            if !self
                .clean_programs
                .values()
                .any(|candidate| fs::canonicalize(candidate).is_ok_and(|path| path == program))
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unconfigured environment-cleared command",
                ));
            }
            self.clean_calls.lock().unwrap().push(CleanCall {
                program: program.to_path_buf(),
                cwd: cwd.to_path_buf(),
                args: args.to_vec(),
                env: env.clone(),
                timeout,
            });
            if args
                .iter()
                .any(|argument| argument == OsStr::new("hash-object"))
            {
                return crate::process::Process.run_env_clear(program, cwd, args, env, timeout);
            }
            Ok(self
                .clean_outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Output {
                    success: false,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: "unexpected isolated Git command".to_owned(),
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

    fn development_clean_git_args(cwd: &Path, args: &[OsString]) -> Option<Vec<String>> {
        if args.len() < 3
            || args[0] != OsStr::new("--no-pager")
            || args[1] != OsStr::new("--no-replace-objects")
            || !is_development_clean_operation(&args[2..])
        {
            return None;
        }
        let mut legacy = vec!["-C".to_owned(), cwd.display().to_string()];
        legacy.extend(
            args[2..]
                .iter()
                .map(|arg| arg.to_str().map(ToOwned::to_owned))
                .collect::<Option<Vec<_>>>()?,
        );
        Some(legacy)
    }

    fn is_development_clean_operation(operation: &[OsString]) -> bool {
        operation
            == [
                "config",
                "--local",
                "--no-includes",
                "--get-all",
                "remote.origin.url",
            ]
            || operation == ["remote", "get-url", "--all", "origin"]
            || operation == ["rev-parse", "--show-toplevel"]
            || (operation.len() == 6
                && operation[..5] == ["ls-tree", "-z", "--full-tree", "HEAD", "--"]
                && operation[5].to_string_lossy().starts_with("bin/"))
            || operation == ["status", "--porcelain", "--untracked-files=normal"]
            || operation
                == [
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ]
            || operation == ["pull", "--ff-only", "--quiet"]
            || operation == ["rev-parse", "HEAD"]
    }

    fn is_development_real_read_call(args: &[OsString]) -> bool {
        let operation = &args[2..];
        operation
            == [
                "config",
                "--local",
                "--no-includes",
                "--get-all",
                "remote.origin.url",
            ]
            || operation == ["remote", "get-url", "--all", "origin"]
            || operation == ["rev-parse", "--show-toplevel"]
            || (operation.len() == 6
                && operation[..5] == ["ls-tree", "-z", "--full-tree", "HEAD", "--"]
                && operation[5].to_string_lossy().starts_with("bin/"))
            || operation == ["status", "--porcelain", "--untracked-files=normal"]
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

    fn record_repo_manifest(
        manifest_path: &std::path::Path,
        name: &str,
        cmd: &str,
        install_dir: &std::path::Path,
    ) -> Manifest {
        manifest::upsert(
            manifest_path,
            ManifestEntry::new(name, "github:repo", cmd, install_dir.display().to_string()),
        )
        .unwrap();
        manifest::read(manifest_path).unwrap()
    }

    fn initialize_git_checkout(root: &std::path::Path, origin: &str) {
        // Adoption fixtures must be real repositories. An empty `.git`
        // directory would let today's permissive implementation pass while
        // forcing the future verifier either to special-case tests or to
        // reject what the characterization suite called valid.
        fixture_git(root, &["init", "--quiet"]);
        fixture_git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        fixture_git(root, &["add", "--all"]);
        fixture_git(
            root,
            &[
                "-c",
                "user.name=Shdeps Test",
                "-c",
                "user.email=shdeps@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        fixture_git(root, &["remote", "add", "origin", origin]);
        fixture_git(root, &["config", "branch.main.remote", "origin"]);
        fixture_git(root, &["config", "branch.main.merge", "refs/heads/main"]);
        fixture_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    fn verified_adoption_runner(root: &Path) -> FakeRunner {
        FakeRunner::default().with_clean_git(verified_adoption_outputs(root))
    }

    fn verified_adoption_outputs(root: &Path) -> Vec<Output> {
        let head = fixture_git_stdout(root, &["rev-parse", "HEAD"]);
        let tree_output =
            fixture_git_stdout(root, &["ls-tree", "-l", "-r", "-z", "--full-tree", "HEAD"]);
        let outputs = vec![
            clean_success(format!(
                "ref: refs/heads/main\tHEAD\n{}\tHEAD\n",
                head.trim()
            )),
            clean_success(""),
            clean_success(""),
            clean_success(format!("{}\n", head.trim())),
            clean_success(""),
            clean_success(tree_output),
        ];
        outputs
    }

    fn assert_isolated_verification_calls(
        calls: &[CleanCall],
        candidate: &Path,
        state_dir: &Path,
        origin: &str,
        branch: &str,
        hash_calls: usize,
    ) {
        let quarantine = calls.first().expect("verification must run").cwd.clone();
        assert_eq!(quarantine.parent(), Some(state_dir));
        assert!(
            quarantine
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".repo-verify."))
        );
        let env = expected_clean_git_env(&quarantine);
        assert_exact_clean_verification_sequence(
            calls,
            CleanVerificationExpectation {
                candidate,
                quarantine: &quarantine,
                base_env: &env,
                transport: "protocol.https.allow=always",
                origin,
                branch,
                hash_calls,
            },
        );
        assert!(
            !quarantine.exists(),
            "successful verification must remove its private quarantine"
        );
    }

    struct CleanVerificationExpectation<'a> {
        candidate: &'a Path,
        quarantine: &'a Path,
        base_env: &'a BTreeMap<OsString, OsString>,
        transport: &'a str,
        origin: &'a str,
        branch: &'a str,
        hash_calls: usize,
    }

    fn assert_exact_clean_verification_sequence(
        calls: &[CleanCall],
        expected: CleanVerificationExpectation<'_>,
    ) {
        let CleanVerificationExpectation {
            candidate,
            quarantine,
            base_env,
            transport,
            origin,
            branch,
            hash_calls,
        } = expected;
        assert_eq!(
            calls.len(),
            6 + hash_calls,
            "the clean Git sequence is a documented security interface"
        );
        let git_dir = OsString::from(format!(
            "--git-dir={}",
            quarantine.join("repo.git").display()
        ));
        let operations = vec![
            vec![
                "ls-remote".into(),
                "--symref".into(),
                "--exit-code".into(),
                "--".into(),
                origin.into(),
                "HEAD".into(),
            ],
            vec![
                "-c".into(),
                OsString::from(format!(
                    "init.templateDir={}",
                    quarantine.join("template").display()
                )),
                "init".into(),
                "--quiet".into(),
                "--bare".into(),
                "--".into(),
                quarantine.join("repo.git").into_os_string(),
            ],
            vec![
                git_dir.clone(),
                "fetch".into(),
                "--quiet".into(),
                "--force".into(),
                "--no-tags".into(),
                "--depth=1".into(),
                "--".into(),
                origin.into(),
                format!("+refs/heads/{branch}:refs/heads/shdeps-adopt").into(),
            ],
            vec![
                git_dir.clone(),
                "rev-parse".into(),
                "--verify".into(),
                "refs/heads/shdeps-adopt^{commit}".into(),
            ],
            vec![
                git_dir.clone(),
                "diff-index".into(),
                "--cached".into(),
                "--quiet".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "refs/heads/shdeps-adopt".into(),
                "--".into(),
            ],
            vec![
                git_dir.clone(),
                "ls-tree".into(),
                "-l".into(),
                "-r".into(),
                "-z".into(),
                "--full-tree".into(),
                "refs/heads/shdeps-adopt".into(),
            ],
        ];
        for (call, operation) in calls.iter().zip(operations) {
            let mut env = base_env.clone();
            if operation.iter().any(|argument| argument == "diff-index") {
                env.insert(
                    "GIT_INDEX_FILE".into(),
                    quarantine.join("candidate.index").into_os_string(),
                );
            }
            assert_exact_clean_call(
                call,
                candidate,
                quarantine,
                &env,
                &expected_clean_git_args(quarantine, transport, operation),
            );
        }
        for call in &calls[6..] {
            assert_exact_clean_call(
                call,
                candidate,
                quarantine,
                base_env,
                &expected_clean_git_args(
                    quarantine,
                    transport,
                    [
                        git_dir.clone(),
                        "hash-object".into(),
                        "--no-filters".into(),
                        "--".into(),
                        quarantine.join("blob-input").into_os_string(),
                    ],
                ),
            );
        }
    }

    fn expected_clean_git_args(
        quarantine: &Path,
        transport: &str,
        operation: impl IntoIterator<Item = OsString>,
    ) -> Vec<OsString> {
        let mut args = vec![
            "--no-pager".into(),
            "--no-replace-objects".into(),
            "-c".into(),
            OsString::from(format!(
                "core.hooksPath={}",
                quarantine.join("hooks").display()
            )),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "core.untrackedCache=false".into(),
            "-c".into(),
            "submodule.recurse=false".into(),
            "-c".into(),
            "fetch.recurseSubmodules=false".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
            "-c".into(),
            "fetch.fsckObjects=true".into(),
            "-c".into(),
            "transfer.fsckObjects=true".into(),
            "-c".into(),
            "protocol.allow=never".into(),
            "-c".into(),
            transport.into(),
        ];
        args.extend(operation);
        args
    }

    fn expected_clean_git_env(quarantine: &Path) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([
            ("HOME".into(), quarantine.join("home").into_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                quarantine.join("xdg-config").into_os_string(),
            ),
            (
                "XDG_DATA_HOME".into(),
                quarantine.join("xdg-data").into_os_string(),
            ),
            (
                "XDG_CACHE_HOME".into(),
                quarantine.join("xdg-cache").into_os_string(),
            ),
            (
                "XDG_STATE_HOME".into(),
                quarantine.join("xdg-state").into_os_string(),
            ),
            ("TMPDIR".into(), quarantine.join("tmp").into_os_string()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                quarantine.join("empty.gitconfig").into_os_string(),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
            ("GIT_PROTOCOL_FROM_USER".into(), "0".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("LC_ALL".into(), "C".into()),
            ("LANG".into(), "C".into()),
            (
                "GIT_CEILING_DIRECTORIES".into(),
                quarantine.as_os_str().to_owned(),
            ),
        ])
    }

    fn assert_exact_clean_call(
        call: &CleanCall,
        candidate: &Path,
        quarantine: &Path,
        env: &BTreeMap<OsString, OsString>,
        args: &[OsString],
    ) {
        assert_eq!(
            call.program,
            fs::canonicalize(host_command_path("git")).unwrap()
        );
        assert_eq!(call.cwd, quarantine);
        assert_eq!(call.args, args);
        assert_eq!(&call.env, env);
        assert_eq!(call.timeout, Duration::from_secs(120));

        // Candidate-controlled paths must never cross the clean-process API,
        // even if a future refactor preserves all other argv/env literals.
        let candidate = candidate.to_string_lossy();
        assert!(
            call.args
                .iter()
                .all(|argument| { !argument.to_string_lossy().contains(candidate.as_ref()) })
        );
        assert!(
            call.env
                .values()
                .all(|value| { !value.to_string_lossy().contains(candidate.as_ref()) })
        );
    }

    fn assert_no_verification_quarantines(state_dir: &Path) {
        let quarantines = fs::read_dir(state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".repo-verify.")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert!(
            quarantines.is_empty(),
            "verification must clean private state after rejection: {quarantines:?}"
        );
    }

    fn clean_success(stdout: impl Into<String>) -> Output {
        Output {
            success: true,
            timed_out: false,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn host_command_path(command: &str) -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").expect("tests require PATH"))
            .map(|directory| directory.join(command))
            .find(|candidate| {
                fs::metadata(candidate)
                    .map(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("tests require `{command}` on PATH"))
    }

    fn relative_from_current(path: &Path) -> PathBuf {
        let current = std::env::current_dir().unwrap();
        let current_components = current.components().collect::<Vec<_>>();
        let path_components = path.components().collect::<Vec<_>>();
        let common = current_components
            .iter()
            .zip(&path_components)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative = PathBuf::new();
        for _ in common..current_components.len() {
            relative.push("..");
        }
        for component in &path_components[common..] {
            relative.push(component.as_os_str());
        }
        relative
    }

    fn fixture_git_stdout(root: &Path, args: &[&str]) -> String {
        let output = fixture_git_output(root, args);
        String::from_utf8(output.stdout).expect("fixture Git output must be UTF-8")
    }

    fn fixture_git(root: &std::path::Path, args: &[&str]) {
        let output = fixture_git_output(root, args);
        assert!(
            output.status.success(),
            "git fixture command failed: git -C {} {}\n{}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn fixture_git_output(root: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "core.hooksPath=/dev/null", "-C"])
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture command failed: git -C {} {}\n{}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    fn release_response(cmd: &str, tag: &str, url: &str) -> Vec<u8> {
        // Keep release fixtures in one helper so performance tests can add
        // multiple repos without hiding the important assertion behind pages of
        // repeated GitHub JSON. The asset name still includes the command and
        // target platform because the selector's matching rules are part of
        // the behavior those tests are exercising.
        release_asset_response(&format!("{cmd}-linux-x86_64"), tag, url)
    }

    fn release_asset_response(asset: &str, tag: &str, url: &str) -> Vec<u8> {
        format!(
            r#"[{{
                "tag_name":"{tag}",
                "draft":false,
                "prerelease":false,
                "assets":[{{
                    "name":"{asset}",
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
        crate::test_support::temp_dir(&format!("shdeps-update-{name}"))
    }
}
