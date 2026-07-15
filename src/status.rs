//! Dependency status classification for `list` and `check`.
//!
//! The Bash CLI has status logic embedded directly in command handlers. The
//! Rust port keeps it separate because `list`, `check`, and future library API
//! callers all need the same method-specific answers, but they do not all have
//! the same performance budget. In particular, `check` must classify the target
//! before package-manager detection or hook sourcing so manifest-backed deps
//! stay cheap.

use std::collections::BTreeMap;
use std::path::Path;

use crate::Result;
use crate::config::{self, Entry};
use crate::jobs;
use crate::manifest::Manifest;
use crate::method;
use crate::platform::{self, RuntimeEnv};
use crate::process::{self, Runner};
use crate::repo;
use crate::runtime::Roots;

/// Why a configured dependency was skipped instead of checked for install state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The dependency's `os:` filter excludes the current platform.
    Platform,
    /// The dependency's `host:` filter excludes the current host.
    Host,
    /// The active package-manager override resolves to `NONE`.
    PackageManager,
}

/// Install status for one configured dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Dependency is installed; optional detail usually carries a version.
    Installed {
        /// Human-readable detail text printed in the `DETAILS` column or check suffix.
        detail: Option<String>,
    },
    /// Dependency is configured but not currently installed.
    Missing,
    /// Dependency is intentionally skipped under the current runtime.
    Skipped(SkipReason),
}

/// Status row for one dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyStatus {
    /// Canonical dependency name.
    pub name: String,
    /// Install method from config.
    pub method: String,
    /// Classified state.
    pub state: State,
}

/// Hook-backed custom status probe.
///
/// Custom hooks remain trusted user code and will eventually run through the
/// Bash compatibility prelude. The status resolver only needs the answer, so it
/// depends on this narrow trait instead of knowing how hooks are sourced.
pub trait CustomProbe {
    /// Returns the custom dependency's installed detail, or `None` when missing.
    fn installed_detail(&self, entry: &Entry, roots: &Roots) -> Result<Option<String>>;
}

/// Probe that treats all custom dependencies as missing.
///
/// This is useful while building non-hook status plumbing and for tests that
/// intentionally focus on config, manifest, and package behavior. The real CLI
/// should use the Bash hook probe before `custom` status is user-facing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCustomProbe;

impl CustomProbe for NoCustomProbe {
    fn installed_detail(&self, _entry: &Entry, _roots: &Roots) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Shared inputs used while classifying dependency status.
///
/// This keeps the public resolver API compact and makes the expensive inputs
/// visible at the call site. A `check` caller can deliberately pass an empty
/// package-version map for manifest-backed deps, while `list` can detect the
/// manager and batch-load package versions once before walking all entries.
pub struct Context<'a, R, C>
where
    R: Runner,
    C: CustomProbe,
{
    /// Runtime filesystem roots.
    pub roots: &'a Roots,
    /// Runtime platform/host identity.
    pub env: &'a RuntimeEnv,
    /// Loaded install manifest.
    pub manifest: &'a Manifest,
    /// Host process runner.
    pub runner: &'a R,
    /// Custom hook status probe.
    pub custom: &'a C,
    /// Detected package manager, or empty when intentionally not detected.
    pub pkg_mgr: &'a str,
    /// Batch-loaded package versions keyed by resolved package name.
    pub package_versions: &'a BTreeMap<String, String>,
}

/// Classifies all configured dependencies in already-loaded config order.
pub fn list<R, C>(entries: &[Entry], context: &Context<'_, R, C>) -> Result<Vec<DependencyStatus>>
where
    R: Runner,
    C: CustomProbe,
{
    entries
        .iter()
        .map(|entry| classify(entry, context))
        .collect()
}

/// Classifies all configured dependencies with bounded read-only parallelism.
///
/// Built-in status probes are read-only: command lookup, `--version`, manifest
/// checks, package database probes, and `git rev-parse`. Those can safely
/// overlap as long as final rows are emitted in config order. Custom hooks are
/// different because they are user-authored shell code; even an `exists()`
/// hook can touch files or depend on side effects. To preserve that observable
/// ordering, this function parallelizes only contiguous non-custom runs and
/// executes each custom entry at its original position.
pub fn list_with_jobs<R, C>(
    entries: &[Entry],
    context: &Context<'_, R, C>,
    max_jobs: usize,
) -> Result<Vec<DependencyStatus>>
where
    R: Runner + Sync,
    C: CustomProbe + Sync,
{
    let mut statuses = Vec::with_capacity(entries.len());
    let mut index = 0;
    while index < entries.len() {
        if entries[index].method == method::CUSTOM {
            statuses.push(classify(&entries[index], context)?);
            index += 1;
            continue;
        }

        let start = index;
        while index < entries.len() && entries[index].method != method::CUSTOM {
            index += 1;
        }

        for status in jobs::parallel_map(&entries[start..index], max_jobs, |entry| {
            classify(entry, context)
        }) {
            statuses.push(status?);
        }
    }
    Ok(statuses)
}

/// Classifies one configured dependency.
///
/// Callers choose whether `pkg_mgr` has already been detected. Passing an empty
/// string intentionally mirrors Bash's "unknown manager" behavior for package
/// alias resolution; the public `check` command should detect it only after it
/// has confirmed that the target is a `pkg` dependency.
pub fn classify<R, C>(entry: &Entry, context: &Context<'_, R, C>) -> Result<DependencyStatus>
where
    R: Runner,
    C: CustomProbe,
{
    Ok(DependencyStatus {
        name: entry.name.clone(),
        method: entry.method.clone(),
        state: state(entry, context)?,
    })
}

fn state<R, C>(entry: &Entry, context: &Context<'_, R, C>) -> Result<State>
where
    R: Runner,
    C: CustomProbe,
{
    match platform::filter_match(&entry.filter, context.env).exit_code() {
        1 => return Ok(State::Skipped(SkipReason::Platform)),
        2 => return Ok(State::Skipped(SkipReason::Host)),
        _ => {}
    }

    match entry.method.as_str() {
        method::PKG => Ok(pkg_state(
            entry,
            context.runner,
            context.pkg_mgr,
            context.package_versions,
            context.env,
        )),
        method::GITHUB_REPO => Ok(github_repo_state(entry, context.roots, context.runner)),
        binary if method::is_binary_install_root(binary) => Ok(manifest_backed_state(
            entry,
            context.manifest,
            context.runner,
        )),
        method::CUSTOM => context
            .custom
            .installed_detail(entry, context.roots)
            .map(installed_or_missing),
        _ => Ok(State::Missing),
    }
}

fn pkg_state(
    entry: &Entry,
    runner: &impl Runner,
    pkg_mgr: &str,
    package_versions: &BTreeMap<String, String>,
    env: &RuntimeEnv,
) -> State {
    let resolved = config::resolve_override_for_runtime(
        &entry.name,
        &entry.aliases,
        Some(pkg_mgr),
        env.is_android(),
    );
    if resolved == "NONE" {
        return State::Skipped(SkipReason::PackageManager);
    }

    if !process::dep_exists_with_versions(runner, &entry.cmd, &resolved, pkg_mgr, package_versions)
    {
        return State::Missing;
    }

    // Command probes are the first Bash detail source; package database output
    // is the fallback. The map is supplied by the command layer so `list` can
    // batch the expensive package query once instead of this resolver hiding a
    // subprocess per dependency.
    let detail = process::dep_version(runner, &entry.cmd)
        .or_else(|| package_versions.get(&resolved).cloned());
    State::Installed { detail }
}

fn github_repo_state(entry: &Entry, roots: &Roots, runner: &impl Runner) -> State {
    let root = roots.install_dir.join(&entry.name);
    if !root.is_dir() {
        return State::Missing;
    }
    if repo::missing_explicit_command(entry, &root) {
        return State::Missing;
    }

    State::Installed {
        detail: repo::version(&root, runner),
    }
}

fn manifest_backed_state(entry: &Entry, manifest: &Manifest, runner: &impl Runner) -> State {
    let Some(manifest_entry) = manifest.get(&entry.name) else {
        return State::Missing;
    };
    if manifest_entry.install_path.is_empty()
        || !process::executable_path(Path::new(&manifest_entry.install_path))
    {
        return State::Missing;
    }

    State::Installed {
        detail: process::dep_version(runner, &entry.cmd),
    }
}

fn installed_or_missing(detail: Option<String>) -> State {
    match detail {
        Some(detail) => State::Installed {
            detail: non_empty(detail),
        },
        None => State::Missing,
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        Context, CustomProbe, DependencyStatus, NoCustomProbe, SkipReason, State, classify, list,
        list_with_jobs,
    };
    use crate::Result;
    use crate::config::parse_entry;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;

    #[derive(Debug, Default)]
    struct FakeRunner {
        commands: BTreeSet<String>,
        outputs: BTreeMap<(String, Vec<String>), Output>,
        delay: Option<Duration>,
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        max_active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeRunner {
        fn with_command(mut self, command: &str) -> Self {
            self.commands.insert(command.to_owned());
            self
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = Some(delay);
            self
        }

        fn max_active(&self) -> usize {
            self.max_active.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn with_output<const N: usize>(
            mut self,
            program: &str,
            args: [&str; N],
            stdout: &str,
        ) -> Self {
            self.outputs.insert(
                (
                    program.to_owned(),
                    args.into_iter().map(str::to_owned).collect(),
                ),
                Output {
                    success: true,
                    timed_out: false,
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                },
            );
            self
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
            self.outputs
                .get(&(
                    program.to_owned(),
                    args.iter().copied().map(str::to_owned).collect(),
                ))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
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

    struct FakeCustom {
        details: BTreeMap<String, Option<String>>,
    }

    impl CustomProbe for FakeCustom {
        fn installed_detail(
            &self,
            entry: &crate::config::Entry,
            _roots: &Roots,
        ) -> Result<Option<String>> {
            Ok(self.details.get(&entry.name).cloned().flatten())
        }
    }

    #[test]
    fn filtered_dependencies_are_skipped_before_method_checks() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let status = classify(
            &parse_entry("tool|pkg|tool|-|os:mac", Some("apt")),
            &context,
        )
        .unwrap();

        assert_eq!(status.state, State::Skipped(SkipReason::Platform));
    }

    #[test]
    fn package_none_alias_reports_package_manager_skip() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let status =
            classify(&parse_entry("font|pkg|-|apt:NONE|-", Some("apt")), &context).unwrap();

        assert_eq!(status.state, State::Skipped(SkipReason::PackageManager));
    }

    #[test]
    fn package_command_reports_installed_with_command_version() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default().with_command("bat").with_output(
            "bat",
            ["--version"],
            "bat 1.2.3\n",
        );
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let status = classify(&parse_entry("bat|pkg|bat|-|-", Some("apt")), &context).unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("1.2.3".to_owned())
            }
        );
    }

    #[test]
    fn package_status_uses_batched_package_version_as_fallback() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default().with_command("font-tool");
        let env = RuntimeEnv::new("linux", "workstation");
        let mut package_versions = BTreeMap::new();
        package_versions.insert("font-package".to_owned(), "4.5.6-1".to_owned());
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let status = classify(
            &parse_entry("font-package|pkg|font-tool|-|-", Some("apt")),
            &context,
        )
        .unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("4.5.6-1".to_owned())
            }
        );
    }

    #[test]
    fn package_status_uses_batched_package_presence_before_subprocess_fallback() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let mut package_versions = BTreeMap::new();
        package_versions.insert("font-package".to_owned(), "4.5.6-1".to_owned());
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let status = classify(
            &parse_entry("font-package|pkg|-|-|-", Some("apt")),
            &context,
        )
        .unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("4.5.6-1".to_owned())
            }
        );
    }

    #[test]
    fn github_repo_reports_version_file_before_git_commit() {
        let roots = roots();
        let repo = roots.install_dir.join("cgraf78/tool");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("VERSION"), "2.0.0\n").unwrap();
        let manifest = Manifest::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let runner = FakeRunner::default();
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "",
            &package_versions,
        );

        let status = classify(
            &parse_entry("cgraf78/tool|github:repo|-|-|-", None),
            &context,
        )
        .unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("2.0.0".to_owned())
            }
        );
    }

    #[test]
    fn github_repo_reports_short_git_commit_without_version_file() {
        let roots = roots();
        let repo = roots.install_dir.join("cgraf78/tool");
        fs::create_dir_all(&repo).unwrap();
        let manifest = Manifest::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let runner = FakeRunner::default().with_output(
            "git",
            ["-C", repo.to_str().unwrap(), "rev-parse", "--short", "HEAD"],
            "abc1234\n",
        );
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "",
            &package_versions,
        );

        let status = classify(
            &parse_entry("cgraf78/tool|github:repo|-|-|-", None),
            &context,
        )
        .unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("commit abc1234".to_owned())
            }
        );
    }

    #[test]
    fn github_repo_with_explicit_command_requires_repo_bin() {
        let roots = roots();
        let repo = roots.install_dir.join("smallstep/cli");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("VERSION"), "0.30.6\n").unwrap();
        let manifest = Manifest::default();
        let env = RuntimeEnv::new("linux", "workstation");
        let runner = FakeRunner::default();
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "",
            &package_versions,
        );

        let status = classify(
            &parse_entry("smallstep/cli|github:repo|step|-|-", None),
            &context,
        )
        .unwrap();

        assert_eq!(status.state, State::Missing);
    }

    #[test]
    fn manifest_backed_methods_require_executable_manifest_path() {
        let roots = roots();
        let bin = roots.bin_dir.join("tool");
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();

        let mut manifest = Manifest::default();
        manifest.upsert(ManifestEntry::new(
            "tool",
            "github:release",
            "tool",
            bin.display().to_string(),
        ));
        let runner = FakeRunner::default().with_output("tool", ["--version"], "tool 3.4.5\n");
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "",
            &package_versions,
        );

        let status =
            classify(&parse_entry("tool|github:release|tool|-|-", None), &context).unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("3.4.5".to_owned())
            }
        );
    }

    #[test]
    fn custom_probe_supplies_status_without_leaking_hook_details() {
        let roots = roots();
        let mut details = BTreeMap::new();
        details.insert("local-tool".to_owned(), Some("9.9.9".to_owned()));
        let custom = FakeCustom { details };
        let env = RuntimeEnv::new("linux", "workstation");
        let manifest = Manifest::default();
        let runner = FakeRunner::default();
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &custom,
            "",
            &package_versions,
        );

        let status = classify(&parse_entry("local-tool|custom|-|-|-", None), &context).unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("9.9.9".to_owned())
            }
        );
    }

    #[test]
    fn list_preserves_loaded_config_order() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default().with_command("a");
        let entries = vec![
            parse_entry("b|pkg|b|-|os:mac", Some("apt")),
            parse_entry("a|pkg|a|-|-", Some("apt")),
        ];
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let statuses = list(&entries, &context).unwrap();

        assert_eq!(
            statuses,
            vec![
                DependencyStatus {
                    name: "b".to_owned(),
                    method: "pkg".to_owned(),
                    state: State::Skipped(SkipReason::Platform),
                },
                DependencyStatus {
                    name: "a".to_owned(),
                    method: "pkg".to_owned(),
                    state: State::Installed { detail: None },
                },
            ]
        );
    }

    #[test]
    fn list_with_jobs_parallelizes_builtin_probes_and_preserves_order() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default()
            .with_command("a")
            .with_command("b")
            .with_output("a", ["--version"], "a 1.0.0\n")
            .with_output("b", ["--version"], "b 2.0.0\n")
            .with_delay(Duration::from_millis(40));
        let entries = vec![
            parse_entry("b|pkg|b|-|-", Some("apt")),
            parse_entry("a|pkg|a|-|-", Some("apt")),
        ];
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        let statuses = list_with_jobs(&entries, &context, 2).unwrap();

        assert_eq!(
            statuses,
            vec![
                DependencyStatus {
                    name: "b".to_owned(),
                    method: "pkg".to_owned(),
                    state: State::Installed {
                        detail: Some("2.0.0".to_owned())
                    },
                },
                DependencyStatus {
                    name: "a".to_owned(),
                    method: "pkg".to_owned(),
                    state: State::Installed {
                        detail: Some("1.0.0".to_owned())
                    },
                },
            ]
        );
        assert_eq!(
            runner.max_active(),
            2,
            "read-only list probes should overlap up to SHDEPS_JOBS while final rows stay in config order"
        );
    }

    #[test]
    fn list_with_jobs_one_keeps_builtin_probes_sequential() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default()
            .with_command("a")
            .with_command("b")
            .with_output("a", ["--version"], "a 1.0.0\n")
            .with_output("b", ["--version"], "b 2.0.0\n")
            .with_delay(Duration::from_millis(10));
        let entries = vec![
            parse_entry("a|pkg|a|-|-", Some("apt")),
            parse_entry("b|pkg|b|-|-", Some("apt")),
        ];
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
            &package_versions,
        );

        list_with_jobs(&entries, &context, 1).unwrap();

        assert_eq!(
            runner.max_active(),
            1,
            "SHDEPS_JOBS=1 must remain the deterministic list escape hatch"
        );
    }

    #[test]
    fn list_with_jobs_keeps_custom_hooks_as_ordering_boundaries() {
        let roots = roots();
        let manifest = Manifest::default();
        let runner = FakeRunner::default()
            .with_command("a")
            .with_command("b")
            .with_output("a", ["--version"], "a 1.0.0\n")
            .with_output("b", ["--version"], "b 2.0.0\n")
            .with_delay(Duration::from_millis(10));
        let mut details = BTreeMap::new();
        details.insert("custom-tool".to_owned(), Some("custom 3.0.0".to_owned()));
        let custom = FakeCustom { details };
        let entries = vec![
            parse_entry("a|pkg|a|-|-", Some("apt")),
            parse_entry("custom-tool|custom|-|-|-", Some("apt")),
            parse_entry("b|pkg|b|-|-", Some("apt")),
        ];
        let env = RuntimeEnv::new("linux", "workstation");
        let package_versions = BTreeMap::new();
        let context = context(
            &roots,
            &env,
            &manifest,
            &runner,
            &custom,
            "apt",
            &package_versions,
        );

        let statuses = list_with_jobs(&entries, &context, 4).unwrap();

        assert_eq!(
            statuses
                .iter()
                .map(|status| status.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "custom-tool", "b"]
        );
        assert_eq!(
            runner.max_active(),
            1,
            "custom hooks are user shell code, so they split parallel builtin runs instead of being reordered around them"
        );
    }

    fn context<'a, C>(
        roots: &'a Roots,
        env: &'a RuntimeEnv,
        manifest: &'a Manifest,
        runner: &'a FakeRunner,
        custom: &'a C,
        pkg_mgr: &'a str,
        package_versions: &'a BTreeMap<String, String>,
    ) -> Context<'a, FakeRunner, C>
    where
        C: CustomProbe,
    {
        Context {
            roots,
            env,
            manifest,
            runner,
            custom,
            pkg_mgr,
            package_versions,
        }
    }

    fn roots() -> Roots {
        let root = temp_dir("status");
        Roots {
            conf_dir: root.join("config"),
            hooks_dir: root.join("config/hooks.d"),
            state_dir: root.join("state"),
            git_dev_dir: root.join("git"),
            install_dir: root.join("share"),
            bin_dir: root.join("bin"),
            home: root,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("shdeps-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
