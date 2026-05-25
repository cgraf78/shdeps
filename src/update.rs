//! `shdeps update` orchestration.
//!
//! Install methods are intentionally small units, but `update` owns the order
//! that makes the system safe: prove or stage the new method before old-method
//! cleanup, update manifest rows only after a method has made its decision, and
//! run post hooks only for dependencies that actually changed. Keeping those
//! rules here avoids each install method learning partial transaction policy.

use std::collections::{BTreeMap, BTreeSet};

use crate::cleanup;
use crate::config::Entry;
use crate::hooks::{BashCustomProbe, Install, Post, Txn};
use crate::http::Client;
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::platform::{self, RuntimeEnv};
use crate::process::Runner;
use crate::runtime::Roots;
use crate::stamp;
use crate::update_external;
use crate::update_pkg;
use crate::update_release;
use crate::update_repo;
use crate::update_transition;
use crate::Result;

/// Options controlling one update run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Force reinstall of dependencies that already appear installed.
    pub reinstall: bool,
    /// Bypass remote freshness stamps.
    pub force: bool,
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
    let transitions = update_transition::by_name(manifest, entries, context.roots)?;

    let mut summary = Summary::default();
    let mut changed = Vec::new();
    let mut queued = Vec::new();
    let hook_txn = Txn::new(&context.roots.state_dir)?;

    update_pkg::prepare(entries, context);

    for entry in entries {
        if entry.method != "pkg" || !active(entry, context.env) {
            continue;
        }

        let item = update_pkg::install(entry, context, &mut queued)?;
        if !item.failed {
            match item.detail.as_str() {
                "installed" => cleanup_successful_transition(
                    entry,
                    transitions.get(&entry.name),
                    context,
                    &mut summary,
                )?,
                "not available" => update_transition::restore_failed(
                    transitions.get(&entry.name),
                    context.manifest_path,
                )?,
                _ => {}
            }
        }
        if item.changed {
            changed.push(entry.name.clone());
        }
        if item.failed {
            summary.failed.push(entry.name.clone());
        }
        summary.items.push(item);
    }

    let pkg_changed_start = changed.len();
    let pkg_failed_start = summary.failed.len();
    update_pkg::flush(&queued, context, &mut changed, &mut summary)?;
    let successful_packages = changed[pkg_changed_start..]
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let failed_packages = summary.failed[pkg_failed_start..]
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
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
            update_transition::restore_failed(transitions.get(&item.name), context.manifest_path)?;
        }
    }

    let release_entries = entries
        .iter()
        .filter(|entry| entry.method == "github:release" && active(entry, context.env))
        .collect::<Vec<_>>();
    let release_prefetch = update_release::prefetch(
        &release_entries,
        context.roots,
        context.env,
        context.env_vars,
        context.runner,
        context.client,
        options,
    );

    for entry in entries {
        if entry.method == "pkg" || !active(entry, context.env) {
            continue;
        }

        match entry.method.as_str() {
            "github:release" => {
                let item = update_transition::install_with_prepared(
                    entry,
                    transitions.get(&entry.name),
                    context.roots,
                    || {
                        update_release::install_with_prefetch(
                            entry,
                            context,
                            options,
                            &release_prefetch,
                        )
                    },
                )?;
                if !item.failed {
                    cleanup_successful_transition(
                        entry,
                        transitions.get(&entry.name),
                        context,
                        &mut summary,
                    )?;
                }
                if item.failed {
                    summary.failed.push(entry.name.clone());
                }
                if item.changed {
                    changed.push(entry.name.clone());
                }
                summary.items.push(item);
            }
            "github:repo" => {
                let item = update_transition::install_with_prepared(
                    entry,
                    transitions.get(&entry.name),
                    context.roots,
                    || update_repo::install(entry, context, options),
                )?;
                if !item.failed {
                    cleanup_successful_transition(
                        entry,
                        transitions.get(&entry.name),
                        context,
                        &mut summary,
                    )?;
                }
                if item.failed {
                    summary.failed.push(entry.name.clone());
                }
                if item.changed {
                    changed.push(entry.name.clone());
                }
                summary.items.push(item);
            }
            "cargo" | "go" | "uv" | "npm" => {
                let item = update_transition::install_with_prepared(
                    entry,
                    transitions.get(&entry.name),
                    context.roots,
                    || update_external::install(entry, context, options),
                )?;
                if !item.failed {
                    cleanup_successful_transition(
                        entry,
                        transitions.get(&entry.name),
                        context,
                        &mut summary,
                    )?;
                }
                if item.failed {
                    summary.failed.push(entry.name.clone());
                }
                if item.changed {
                    changed.push(entry.name.clone());
                }
                summary.items.push(item);
            }
            "custom" => {
                let outcome = install_custom(
                    entry,
                    context.manifest_path,
                    context.roots,
                    context.hooks,
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
    run_post_hooks(
        &changed,
        context.roots,
        context.hooks,
        &hook_txn,
        &mut summary,
    )?;
    Ok(summary)
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
    manifest_path: &std::path::Path,
    roots: &Roots,
    hooks: &BashCustomProbe,
    txn: &Txn,
    options: Options,
    transition: Option<&ManifestEntry>,
) -> Result<CustomOutcome> {
    let install = hooks.install_with_txn(&entry.name, roots, options.reinstall, Some(txn))?;
    let marked = txn.collect()?;
    match install {
        Install::Already { detail } => {
            let mut outcome =
                successful_custom(entry, manifest_path, roots, false, detail, transition)?;
            outcome.marked = marked;
            Ok(outcome)
        }
        Install::Installed { detail } => {
            let mut outcome =
                successful_custom(entry, manifest_path, roots, true, detail, transition)?;
            outcome.marked = marked;
            Ok(outcome)
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
                marked,
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
            marked,
        }),
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

    use super::{run, Context, Options};
    use bzip2::write::BzEncoder;
    use bzip2::Compression as BzCompression;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::{Builder, Header};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    use crate::config::parse_entry;
    use crate::hooks::BashCustomProbe;
    use crate::http::Client;
    use crate::link_state::{self, Kind};
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
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("ripgrep")
            .is_none());
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://example/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64", b"binary".to_vec());
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
    fn update_github_release_sends_token_to_metadata_and_asset_download() {
        let mut fixture = Fixture::new("release-token");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("GH_TOKEN".to_owned(), "ci-token".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://example/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64", b"binary".to_vec());
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
                    "https://api.github.com/repos/owner/tool/releases".to_owned(),
                    Some("ci-token".to_owned())
                ),
                (
                    "https://example/tool-linux-x86_64".to_owned(),
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
            .with_delay(Duration::from_millis(25))
            .with(
                "https://api.github.com/repos/owner/tool-a/releases",
                release_response("tool-a", "v1.0.0", "https://example/tool-a-linux-x86_64"),
            )
            .with("https://example/tool-a-linux-x86_64", b"tool-a".to_vec())
            .with(
                "https://api.github.com/repos/owner/tool-b/releases",
                release_response("tool-b", "v1.0.0", "https://example/tool-b-linux-x86_64"),
            )
            .with("https://example/tool-b-linux-x86_64", b"tool-b".to_vec())
            .with(
                "https://api.github.com/repos/owner/tool-c/releases",
                release_response("tool-c", "v1.0.0", "https://example/tool-c-linux-x86_64"),
            )
            .with("https://example/tool-c-linux-x86_64", b"tool-c".to_vec());
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
            .filter(|(url, _)| url.starts_with("https://example/tool-"))
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
    fn update_github_release_jobs_one_keeps_remote_checks_sequential() {
        let mut fixture = Fixture::new("release-prefetch-sequential");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "1".to_owned());
        fixture.client = FakeClient::default()
            .with_delay(Duration::from_millis(5))
            .with(
                "https://api.github.com/repos/owner/tool-a/releases",
                release_response("tool-a", "v1.0.0", "https://example/tool-a-linux-x86_64"),
            )
            .with("https://example/tool-a-linux-x86_64", b"tool-a".to_vec())
            .with(
                "https://api.github.com/repos/owner/tool-b/releases",
                release_response("tool-b", "v1.0.0", "https://example/tool-b-linux-x86_64"),
            )
            .with("https://example/tool-b-linux-x86_64", b"tool-b".to_vec());
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
    fn update_github_release_reuses_gh_token_across_metadata_and_assets() {
        let mut fixture = Fixture::new("release-token-prefetch");
        fixture.write_lib();
        fixture
            .env_vars
            .insert("SHDEPS_JOBS".to_owned(), "1".to_owned());
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool-a/releases",
                release_response("tool-a", "v1.0.0", "https://example/tool-a-linux-x86_64"),
            )
            .with("https://example/tool-a-linux-x86_64", b"tool-a".to_vec())
            .with(
                "https://api.github.com/repos/owner/tool-b/releases",
                release_response("tool-b", "v1.0.0", "https://example/tool-b-linux-x86_64"),
            )
            .with("https://example/tool-b-linux-x86_64", b"tool-b".to_vec());
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
            "`gh auth token` can be noticeably slow; shdeps should resolve it once per update run and reuse it for every release request"
        );
        assert!(fixture
            .client
            .requests()
            .iter()
            .all(|(_, token)| token.as_deref() == Some("gh-token")));
    }

    #[test]
    fn update_github_release_decompresses_gzip_single_binary() {
        let mut fixture = Fixture::new("release-gz-single");
        fixture.write_lib();
        fixture.client = FakeClient::default()
            .with(
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.gz",
                        "browser_download_url":"https://example/tool-linux-x86_64.gz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64.gz", gzip(b"binary"));
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.bz2",
                        "browser_download_url":"https://example/tool-linux-x86_64.bz2"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64.bz2", bzip2(b"binary"));
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64.zst",
                        "browser_download_url":"https://example/tool-linux-x86_64.zst"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64.zst", zstd(b"binary"));
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
    fn update_github_release_force_checks_without_reinstalling_current_binary() {
        let mut fixture = Fixture::new("release-force-current");
        fixture.write_lib();
        fixture.client = FakeClient::default().with(
            "https://api.github.com/repos/owner/tool/releases",
            br#"[{
                "tag_name":"v1.2.3",
                "draft":false,
                "prerelease":false,
                "assets":[{
                    "name":"tool-linux-x86_64",
                    "browser_download_url":"https://example/tool-linux-x86_64"
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
                "https://api.github.com/repos/owner/tool/releases".to_owned(),
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-linux-x86_64",
                        "browser_download_url":"https://example/tool-linux-x86_64"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-linux-x86_64", b"new-binary".to_vec());
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
                    "https://api.github.com/repos/owner/tool/releases".to_owned(),
                    None
                ),
                ("https://example/tool-linux-x86_64".to_owned(), None)
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.tar.gz",
                        "browser_download_url":"https://example/tool-v1.2.3-linux-x86_64.tar.gz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-v1.2.3-linux-x86_64.tar.gz", archive);
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.tar.xz",
                        "browser_download_url":"https://example/tool-v1.2.3-linux-x86_64.tar.xz"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-v1.2.3-linux-x86_64.tar.xz", archive);
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
                "https://api.github.com/repos/owner/tool/releases",
                br#"[{
                    "tag_name":"v1.2.3",
                    "draft":false,
                    "prerelease":false,
                    "assets":[{
                        "name":"tool-v1.2.3-linux-x86_64.zip",
                        "browser_download_url":"https://example/tool-v1.2.3-linux-x86_64.zip"
                    }]
                }]"#
                .to_vec(),
            )
            .with("https://example/tool-v1.2.3-linux-x86_64.zip", archive);
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
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("cgraf78/ds")
            .is_some());
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("cgraf78/ds.git")
            .is_none());
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
        assert!(manifest::read(&manifest_path)
            .unwrap()
            .get("cgraf78/ds")
            .is_none());
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

    #[derive(Debug, Clone, Default)]
    struct FakeClient {
        responses: std::collections::BTreeMap<String, Vec<u8>>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
        delay: Option<Duration>,
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        max_active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        calls: std::cell::RefCell<Vec<String>>,
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

        fn push_output(&mut self, key: String, output: Output) {
            self.outputs
                .entry(key)
                .or_default()
                .borrow_mut()
                .push_back(output);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
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
            self.calls.borrow_mut().push(key.clone());
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

    type QueuedOutputs = std::cell::RefCell<std::collections::VecDeque<Output>>;

    fn next_output(outputs: &QueuedOutputs) -> Output {
        // Some install flows intentionally run the same command twice, for
        // example `git pull` before and after rewriting an HTTPS origin to SSH.
        // Keep a tiny queue per command key so tests can model those retries
        // without shell scripts or global PATH mutation.
        let mut outputs = outputs.borrow_mut();
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

    fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn tar_xz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let tar = tar(entries);
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
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
