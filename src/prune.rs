//! Orphan detection and cleanup orchestration for `shdeps prune`.
//!
//! Built-in artifact ownership lives in `cleanup`, and arbitrary shell cleanup
//! lives in hook subprocesses. This module is the coordinator that preserves the
//! Bash safety contract: list orphaned manifest rows, refuse an all-orphans
//! wipe unless explicitly confirmed, run optional hook cleanup, then remove
//! shdeps-owned artifacts and manifest tracking.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use crate::Result;
use crate::cleanup;
use crate::config::Entry;
use crate::hooks::{BashCustomProbe, Uninstall};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::runtime;

/// User options for `shdeps prune`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Skip the interactive confirmation prompt.
    pub yes: bool,
    /// Show orphaned dependencies without removing files or state.
    pub dry_run: bool,
    /// Suppress the confirmation prompt and skip action unless `yes` is set.
    pub quiet: bool,
}

/// Result of one orphan cleanup attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Orphaned manifest entry.
    pub entry: ManifestEntry,
    /// Optional hook cleanup result.
    pub hook: Uninstall,
    /// Built-in cleanup decisions.
    pub cleanup: Option<cleanup::Summary>,
    /// Built-in cleanup failed after the hook attempt.
    pub cleanup_error: Option<String>,
}

/// Completed prune operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Orphans detected before any removal.
    pub orphans: Vec<ManifestEntry>,
    /// Items actually attempted.
    pub removed: Vec<Item>,
    /// True when an empty config would orphan every manifest entry and `-y`
    /// was not supplied.
    pub guarded_all_orphans: bool,
    /// True when quiet mode skipped prompt and action.
    pub quiet_skipped: bool,
}

impl Summary {
    /// Returns whether any built-in cleanup attempt failed.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.removed.iter().any(|item| item.cleanup_error.is_some())
    }
}

/// Runs prune using already-loaded config and manifest paths.
pub fn run(
    config: &[Entry],
    manifest: &Manifest,
    manifest_path: &Path,
    roots: &runtime::Roots,
    hooks: &BashCustomProbe,
    options: Options,
) -> Result<Summary> {
    // Hold the same state-directory advisory lock that `update::run`
    // uses. Prune mutates the manifest, link-state files, and the
    // install/bin trees; a concurrent update or another prune run
    // would otherwise see/write inconsistent state. Acquired here
    // (not in dry-run paths below) so even a dry-run reports against
    // a consistent snapshot rather than racing an in-flight update.
    let _lock = crate::state::StateLock::acquire(&roots.state_dir)?;

    let orphans = orphans(manifest, config);
    // The "all orphans" guard prevents a silent bulk-delete of every
    // shdeps-tracked dep without explicit `--yes`. The pre-fix gate
    // (`config.is_empty()`) only caught the literal empty-config case;
    // it missed the equally-dangerous shape where the config is
    // non-empty but every manifest entry is still about to be deleted
    // — e.g., a filtered config whose declared names do not match any
    // currently-tracked dep, or a config that renames everything in
    // one go. Gating on `orphans.len() == manifest.entries().len()`
    // catches every "you're about to delete everything" scenario.
    let prunes_everything =
        !manifest.entries().is_empty() && orphans.len() == manifest.entries().len();
    if prunes_everything && !options.yes {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: true,
            quiet_skipped: false,
        });
    }
    if orphans.is_empty() || options.dry_run {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: false,
            quiet_skipped: false,
        });
    }
    if options.quiet && !options.yes {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: false,
            quiet_skipped: true,
        });
    }

    let cleanup_roots = cleanup_roots(roots);
    let mut removed = Vec::new();
    for entry in &orphans {
        let hook = hooks.uninstall(&entry.name, roots)?;
        let (cleanup, cleanup_error) = match cleanup::remove_builtin(entry, &cleanup_roots) {
            Ok(summary) => (Some(summary), None),
            Err(error) => (None, Some(error.to_string())),
        };

        // Bash removes tracking even when cleanup warns. Preserve that bias so
        // a broken uninstall hook or partially missing filesystem state does not
        // strand the manifest forever. Real filesystem errors are still surfaced
        // through the summary so the CLI exits non-zero.
        manifest::remove(manifest_path, &entry.name)?;
        removed.push(Item {
            entry: entry.clone(),
            hook,
            cleanup,
            cleanup_error,
        });
    }

    Ok(Summary {
        orphans,
        removed,
        guarded_all_orphans: false,
        quiet_skipped: false,
    })
}

fn orphans(manifest: &Manifest, config: &[Entry]) -> Vec<ManifestEntry> {
    let configured = config
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();

    manifest
        .entries()
        .iter()
        .filter(|entry| !configured.contains(entry.name.as_str()))
        .cloned()
        .collect()
}

fn cleanup_roots(roots: &runtime::Roots) -> cleanup::Roots {
    cleanup::Roots {
        state_dir: roots.state_dir.clone(),
        install_dir: roots.install_dir.clone(),
        bin_dir: roots.bin_dir.clone(),
        home: roots.home.clone(),
    }
}

impl fmt::Display for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.entry.name, self.entry.method)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Options, run};
    use crate::config::parse_entry;
    use crate::hooks::{BashCustomProbe, Uninstall};
    use crate::manifest::{self, ManifestEntry};
    use crate::runtime::Roots;

    #[test]
    fn prune_removes_orphan_manifest_and_owned_artifacts() {
        let fixture = Fixture::new("remove-orphan");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin = fixture.roots.bin_dir.join("tool");
        fixture.write(&bin, "#!/bin/sh\n");
        fixture.write(
            &fixture.roots.install_dir.join("owner/tool/artifact"),
            "artifact\n",
        );
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                bin.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(summary.orphans.len(), 1);
        assert_eq!(summary.removed[0].hook, Uninstall::MissingHook);
        assert!(!bin.exists());
        assert!(!fixture.roots.install_dir.join("owner/tool").exists());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    fn prune_guard_requires_yes_when_empty_config_would_remove_everything() {
        let fixture = Fixture::new("guard");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("pkg-tool", "pkg", "pkg-tool", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options::default(),
        )
        .unwrap();

        assert!(summary.guarded_all_orphans);
        assert!(summary.removed.is_empty());
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("pkg-tool"),
            Some(&ManifestEntry::new("pkg-tool", "pkg", "pkg-tool", ""))
        );
    }

    #[test]
    fn dry_run_and_quiet_skip_do_not_touch_manifest() {
        // Two manifest entries: `keep` matches the config and survives;
        // `old` is the orphan. This shape is important because the
        // all-orphans guard fires before the quiet-skip branch, so the
        // quiet behavior only exercises when at least one tracked dep
        // is NOT being deleted.
        let fixture = Fixture::new("dry-run");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("keep", "custom", "keep", ""),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("old", "custom", "old", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let dry = run(
            &[parse_entry("keep|custom", None)],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                dry_run: true,
                quiet: false,
            },
        )
        .unwrap();
        let quiet = run(
            &[parse_entry("keep|custom", None)],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                quiet: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(dry.orphans.len(), 1);
        assert!(dry.removed.is_empty());
        assert!(quiet.quiet_skipped);
        assert!(manifest::read(&manifest_path).unwrap().get("old").is_some());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("keep")
                .is_some()
        );
    }

    #[test]
    fn all_orphans_guard_fires_when_every_tracked_dep_is_about_to_be_pruned() {
        // The pre-fix guard only caught the literal `config.is_empty()`
        // case. Equally dangerous: a non-empty config whose declared
        // names do not match any tracked dep (e.g., everything renamed
        // in one go, or platform filters at a higher layer that drop
        // every survivor). Both shapes now trip the same guard so a
        // silent bulk-delete cannot happen without explicit `--yes`.
        let fixture = Fixture::new("all-orphans-nonempty-config");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("old-a", "pkg", "old-a", ""),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("old-b", "pkg", "old-b", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            // Non-empty config, but neither name matches anything in
            // the manifest — every existing record is an orphan.
            &[parse_entry("new-tool|pkg", None)],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options::default(),
        )
        .unwrap();

        assert!(summary.guarded_all_orphans);
        assert!(summary.removed.is_empty());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("old-a")
                .is_some()
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("old-b")
                .is_some()
        );
    }

    #[test]
    fn prune_runs_optional_uninstall_hook_before_manifest_removal() {
        let fixture = Fixture::new("hook");
        fixture.write(
            &fixture.roots.hooks_dir.join("custom.sh"),
            "uninstall() { printf '%s\\n' \"$1\" > \"$SHDEPS_STATE_DIR/hook-ran\"; }\n",
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("custom", "custom", "custom", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(summary.removed[0].hook, Uninstall::Removed);
        assert_eq!(
            fs::read_to_string(fixture.roots.state_dir.join("hook-ran")).unwrap(),
            "custom\n"
        );
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    struct Fixture {
        roots: Roots,
        hooks: BashCustomProbe,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = temp_dir(name);
            let roots = Roots {
                conf_dir: root.join("config"),
                hooks_dir: root.join("config/hooks.d"),
                state_dir: root.join("state"),
                git_dev_dir: root.join("git"),
                install_dir: root.join("share"),
                bin_dir: root.join("bin"),
                home: root,
            };
            fs::create_dir_all(&roots.hooks_dir).unwrap();
            fs::create_dir_all(&roots.state_dir).unwrap();
            let lib = roots.home.join("shdeps.sh");
            fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
            Self {
                roots,
                hooks: BashCustomProbe::new(lib),
            }
        }

        fn write(&self, path: &PathBuf, content: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "shdeps-prune-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
