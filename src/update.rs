//! `shdeps update` orchestration.
//!
//! Install methods are intentionally small units, but `update` owns the order
//! that makes the system safe: prove or stage the new method before old-method
//! cleanup, update manifest rows only after a method has made its decision, and
//! run post hooks only for dependencies that actually changed. Keeping those
//! rules here avoids each install method learning partial transaction policy.

use std::collections::HashMap;

use crate::cleanup;
use crate::config::Entry;
use crate::hooks::{BashCustomProbe, Install, Post};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::platform::{self, RuntimeEnv};
use crate::process::Runner;
use crate::runtime::Roots;
use crate::update_pkg;
use crate::Result;

/// Options controlling one update run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Force reinstall of dependencies that already appear installed.
    pub reinstall: bool,
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
    /// Human-readable status detail for CLI output.
    pub detail: String,
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
    pub leftovers: Vec<String>,
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
}

impl Summary {
    /// Returns whether the update had any failure.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.failed.is_empty()
    }
}

/// Runs update for already-parsed entries.
pub fn run(
    entries: &[Entry],
    manifest: &Manifest,
    context: &Context<'_, impl Runner>,
    options: Options,
) -> Result<Summary> {
    let transitions = transitions_by_name(manifest, entries);

    let mut summary = Summary::default();
    let mut changed = Vec::new();
    let mut queued = Vec::new();

    for entry in entries {
        if entry.method != "pkg" || !active(entry, context.env) {
            continue;
        }

        let item = update_pkg::install(entry, context, &mut queued)?;
        if item.changed {
            changed.push(entry.name.clone());
        }
        if item.failed {
            summary.failed.push(entry.name.clone());
        }
        summary.items.push(item);
    }

    update_pkg::flush(&queued, context, &mut changed, &mut summary)?;

    for entry in entries {
        if entry.method == "pkg" || !active(entry, context.env) {
            continue;
        }

        match entry.method.as_str() {
            "custom" => {
                let outcome = install_custom(
                    entry,
                    context.manifest_path,
                    context.roots,
                    context.hooks,
                    options,
                    transitions.get(&entry.name),
                )?;
                let item = outcome.item;
                if outcome.cleanup_leftover {
                    summary.leftovers.push(entry.name.clone());
                }
                if item.failed {
                    summary.failed.push(entry.name.clone());
                }
                if item.changed {
                    changed.push(entry.name.clone());
                }
                summary.items.push(item);
            }
            method => {
                summary.failed.push(entry.name.clone());
                summary.items.push(Item {
                    name: entry.name.clone(),
                    changed: false,
                    failed: true,
                    detail: format!("{method} update is not implemented yet"),
                });
            }
        }
    }

    // Post hooks deliberately run after every install decision rather than
    // inline with each method. Many hooks repair shell completions, symlinks,
    // or dependent tools, so they should see the final state for the full
    // update pass instead of an intermediate per-method view.
    run_post_hooks(&changed, context.roots, context.hooks, &mut summary)?;
    Ok(summary)
}

fn active(entry: &Entry, env: &RuntimeEnv) -> bool {
    matches!(
        platform::filter_match(&entry.filter, env),
        platform::FilterMatch::Match
    )
}

fn transitions_by_name(manifest: &Manifest, entries: &[Entry]) -> HashMap<String, ManifestEntry> {
    cleanup::method_transitions(manifest, entries)
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect()
}

struct CustomOutcome {
    item: Item,
    cleanup_leftover: bool,
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
        ManifestEntry::new(&entry.name, "custom", &entry.cmd, ""),
    )?;

    let cleanup_leftover = match transition {
        Some(old) => cleanup_transition(old, roots),
        None => false,
    };

    Ok(CustomOutcome {
        item: Item {
            name: entry.name.clone(),
            changed,
            failed: false,
            detail,
        },
        cleanup_leftover,
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

fn install_custom(
    entry: &Entry,
    manifest_path: &std::path::Path,
    roots: &Roots,
    hooks: &BashCustomProbe,
    options: Options,
    transition: Option<&ManifestEntry>,
) -> Result<CustomOutcome> {
    let install = hooks.install(&entry.name, roots, options.reinstall)?;
    match install {
        Install::Already { detail } => {
            successful_custom(entry, manifest_path, roots, false, detail, transition)
        }
        Install::Installed { detail } => {
            successful_custom(entry, manifest_path, roots, true, detail, transition)
        }
        Install::MissingHook | Install::MissingFunction | Install::SourceFailed => {
            Ok(CustomOutcome {
                item: Item {
                    name: entry.name.clone(),
                    changed: false,
                    failed: false,
                    detail: "custom hook missing or unusable".to_owned(),
                },
                cleanup_leftover: false,
            })
        }
        Install::Failed => Ok(CustomOutcome {
            item: Item {
                name: entry.name.clone(),
                changed: false,
                failed: true,
                detail: "custom install failed".to_owned(),
            },
            cleanup_leftover: false,
        }),
    }
}

fn run_post_hooks(
    changed: &[String],
    roots: &Roots,
    hooks: &BashCustomProbe,
    summary: &mut Summary,
) -> Result<()> {
    for name in changed {
        match hooks.post(name, roots)? {
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
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{run, Context, Options};
    use crate::config::parse_entry;
    use crate::hooks::BashCustomProbe;
    use crate::manifest::{self, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::process::{Output, Runner};
    use crate::runtime::Roots;

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
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("tool")
            .is_none());
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
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("tool")
            .is_none());
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

    struct Fixture {
        roots: Roots,
        hooks: BashCustomProbe,
        env: RuntimeEnv,
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
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    struct FakeRunner {
        commands: std::collections::BTreeSet<String>,
        outputs: std::collections::BTreeMap<String, Output>,
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
            self.outputs.insert(
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

        fn with_failure(
            mut self,
            program: &str,
            args: impl IntoIterator<Item = impl AsRef<str>>,
        ) -> Self {
            self.outputs.insert(
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
            Ok(self
                .outputs
                .get(&key(program, args))
                .cloned()
                .unwrap_or(Output {
                    success: false,
                    timed_out: false,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
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
