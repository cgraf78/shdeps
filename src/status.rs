//! Dependency status classification for `list` and `check`.
//!
//! The Bash CLI has status logic embedded directly in command handlers. The
//! Rust port keeps it separate because `list`, `check`, and future library API
//! callers all need the same method-specific answers, but they do not all have
//! the same performance budget. In particular, `check` must classify the target
//! before package-manager detection or hook sourcing so manifest-backed deps
//! stay cheap.

use std::fs;
use std::path::Path;

use crate::config::{self, Entry};
use crate::manifest::Manifest;
use crate::platform::{self, RuntimeEnv};
use crate::process::{self, Runner};
use crate::runtime::Roots;
use crate::Result;

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

/// Classifies all configured dependencies in already-loaded config order.
pub fn list(
    entries: &[Entry],
    roots: &Roots,
    env: &RuntimeEnv,
    manifest: &Manifest,
    runner: &impl Runner,
    custom: &impl CustomProbe,
    pkg_mgr: &str,
) -> Result<Vec<DependencyStatus>> {
    entries
        .iter()
        .map(|entry| classify(entry, roots, env, manifest, runner, custom, pkg_mgr))
        .collect()
}

/// Classifies one configured dependency.
///
/// Callers choose whether `pkg_mgr` has already been detected. Passing an empty
/// string intentionally mirrors Bash's "unknown manager" behavior for package
/// alias resolution; the public `check` command should detect it only after it
/// has confirmed that the target is a `pkg` dependency.
pub fn classify(
    entry: &Entry,
    roots: &Roots,
    env: &RuntimeEnv,
    manifest: &Manifest,
    runner: &impl Runner,
    custom: &impl CustomProbe,
    pkg_mgr: &str,
) -> Result<DependencyStatus> {
    Ok(DependencyStatus {
        name: entry.name.clone(),
        method: entry.method.clone(),
        state: state(entry, roots, env, manifest, runner, custom, pkg_mgr)?,
    })
}

fn state(
    entry: &Entry,
    roots: &Roots,
    env: &RuntimeEnv,
    manifest: &Manifest,
    runner: &impl Runner,
    custom: &impl CustomProbe,
    pkg_mgr: &str,
) -> Result<State> {
    match platform::filter_match(&entry.filter, env).exit_code() {
        1 => return Ok(State::Skipped(SkipReason::Platform)),
        2 => return Ok(State::Skipped(SkipReason::Host)),
        _ => {}
    }

    match entry.method.as_str() {
        "pkg" => Ok(pkg_state(entry, runner, pkg_mgr)),
        "github:repo" => Ok(github_repo_state(entry, roots, runner)),
        "github:release" | "cargo" | "go" | "uv" | "npm" => {
            Ok(manifest_backed_state(entry, manifest, runner))
        }
        "custom" => custom
            .installed_detail(entry, roots)
            .map(installed_or_missing),
        _ => Ok(State::Missing),
    }
}

fn pkg_state(entry: &Entry, runner: &impl Runner, pkg_mgr: &str) -> State {
    let resolved = config::resolve_override(&entry.name, &entry.aliases, Some(pkg_mgr));
    if resolved == "NONE" {
        return State::Skipped(SkipReason::PackageManager);
    }

    if !process::dep_exists(runner, &entry.cmd, &resolved, pkg_mgr) {
        return State::Missing;
    }

    // Command probes are cheap and match the first Bash detail source. Package
    // database version fallback is deliberately left to the command layer that
    // owns batching; doing per-dependency package DB probes here would make
    // `list` scale badly and violate the warm-path performance plan.
    State::Installed {
        detail: process::dep_version(runner, &entry.cmd),
    }
}

fn github_repo_state(entry: &Entry, roots: &Roots, runner: &impl Runner) -> State {
    let root = roots.install_dir.join(&entry.name);
    if !root.is_dir() {
        return State::Missing;
    }

    State::Installed {
        detail: repo_version(&root, runner),
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

fn repo_version(root: &Path, runner: &impl Runner) -> Option<String> {
    let version_path = root.join("VERSION");
    if let Ok(version) = fs::read_to_string(&version_path) {
        // Current Bash reports VERSION verbatim. Trim only trailing newlines so
        // CLI formatting can add its own line ending without doubling blanks.
        return Some(version.trim_end_matches(['\r', '\n']).to_owned());
    }

    runner
        .run(
            "git",
            &[
                "-C",
                &root.display().to_string(),
                "rev-parse",
                "--short",
                "HEAD",
            ],
            None,
        )
        .ok()
        .filter(|output| output.success)
        .map(|output| format!("commit {}", output.stdout.trim()))
        .and_then(non_empty)
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{classify, list, CustomProbe, DependencyStatus, NoCustomProbe, SkipReason, State};
    use crate::config::parse_entry;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;
    use crate::Result;

    #[derive(Debug, Default)]
    struct FakeRunner {
        commands: BTreeSet<String>,
        outputs: BTreeMap<(String, Vec<String>), Output>,
    }

    impl FakeRunner {
        fn with_command(mut self, command: &str) -> Self {
            self.commands.insert(command.to_owned());
            self
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
            self.outputs
                .get(&(
                    program.to_owned(),
                    args.iter().copied().map(str::to_owned).collect(),
                ))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
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

        let status = classify(
            &parse_entry("tool|pkg|tool|-|os:mac", Some("apt")),
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
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

        let status = classify(
            &parse_entry("font|pkg|-|apt:NONE|-", Some("apt")),
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
        )
        .unwrap();

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

        let status = classify(
            &parse_entry("bat|pkg|bat|-|-", Some("apt")),
            &roots,
            &env,
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
        )
        .unwrap();

        assert_eq!(
            status.state,
            State::Installed {
                detail: Some("1.2.3".to_owned())
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

        let status = classify(
            &parse_entry("cgraf78/tool|github:repo|-|-|-", None),
            &roots,
            &env,
            &manifest,
            &FakeRunner::default(),
            &NoCustomProbe,
            "",
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

        let status = classify(
            &parse_entry("tool|github:release|tool|-|-", None),
            &roots,
            &RuntimeEnv::new("linux", "workstation"),
            &manifest,
            &runner,
            &NoCustomProbe,
            "",
        )
        .unwrap();

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

        let status = classify(
            &parse_entry("local-tool|custom|-|-|-", None),
            &roots,
            &RuntimeEnv::new("linux", "workstation"),
            &Manifest::default(),
            &FakeRunner::default(),
            &custom,
            "",
        )
        .unwrap();

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

        let statuses = list(
            &entries,
            &roots,
            &RuntimeEnv::new("linux", "workstation"),
            &manifest,
            &runner,
            &NoCustomProbe,
            "apt",
        )
        .unwrap();

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
