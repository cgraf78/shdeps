//! Orphan detection and cleanup orchestration for `shdeps prune`.
//!
//! Built-in artifact ownership lives in `cleanup`, and arbitrary shell cleanup
//! lives in hook subprocesses. This module is the coordinator that preserves the
//! Bash safety contract: list orphaned manifest rows, refuse an all-orphans
//! wipe unless explicitly confirmed, run optional hook cleanup, then remove
//! shdeps-owned artifacts and manifest tracking.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::Path;

use crate::Result;
use crate::cancellation;
use crate::cleanup;
use crate::config::Entry;
use crate::hooks::{BashCustomProbe, Uninstall};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::process::{Process, Runner};
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
    cancellation::check()?;
    // A fresh checkout can be durably published before its manifest row and
    // transition journal are finalized. Discover that ownership from the
    // state index, not current config or the caller's stale manifest snapshot.
    // Only take the state lock early when such recovery exists, preserving the
    // normal nonblocking dry-run/empty fast path.
    #[cfg(unix)]
    let mut recovery_lock = if crate::repo_transition::pending_fresh_publications(
        &roots.state_dir,
        &roots.install_dir,
    )?
    .is_empty()
    {
        None
    } else {
        Some(crate::state::StateLock::acquire(&roots.state_dir)?)
    };
    #[cfg(not(unix))]
    let mut recovery_lock: Option<crate::state::StateLock> = None;
    if recovery_lock.is_some() {
        recover_fresh_repo_publications(roots, manifest_path)?;
    }
    let recovered_manifest = recovery_lock
        .as_ref()
        .map(|_| manifest::read(manifest_path))
        .transpose()?;
    let manifest = recovered_manifest.as_ref().unwrap_or(manifest);
    let orphans = orphans(manifest, config);
    // The "all orphans" guard prevents a silent bulk-delete of every
    // shdeps-tracked dep without explicit `--yes`. The pre-fix gate
    // (`config.is_empty()`) only caught the literal empty-config case;
    // it missed the equally-dangerous shape where the config is
    // non-empty but every manifest entry is still about to be deleted
    // — e.g., a filtered config whose declared names do not match any
    // currently-tracked dep, or a config that renames everything in
    // one go. Compare by UNIQUE manifest names rather than raw row
    // counts: the manifest format allows duplicate rows (it is the
    // last-row-wins source of truth), and a comparison against raw
    // `entries().len()` would let a manifest containing one duplicate
    // row for a configured dep silently disarm the guard even when
    // every other tracked dep is about to be pruned.
    let unique_manifest_deps: std::collections::BTreeSet<&str> = manifest
        .effective_entries()
        .into_iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let unique_orphan_deps: std::collections::BTreeSet<&str> =
        orphans.iter().map(|entry| entry.name.as_str()).collect();
    let prunes_everything =
        !unique_manifest_deps.is_empty() && unique_orphan_deps.len() == unique_manifest_deps.len();
    if prunes_everything && !options.yes {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: true,
            quiet_skipped: false,
        });
    }
    if options.dry_run {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: false,
            quiet_skipped: false,
        });
    }
    // A later update OR prune must retry post-swap cleanup. An otherwise
    // orphan-free run still takes the state lock when method-transition state
    // exists so committed cleanups are retried and their journals retired.
    // The probe is intentionally non-validating; strict validation fails
    // closed under the lock inside recovery.
    if orphans.is_empty() && !crate::update_transition::has_pending_durable_transitions(roots) {
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

    // Acquire the state-directory advisory lock only after the
    // dry-run / empty / quiet-skip early-returns. A long `shdeps
    // update` holding the lock can take minutes (network + package
    // manager work); making `prune --dry-run` block on that lock
    // would be a UX regression for what is supposed to be a fast
    // read-only preview. Real mutation paths below this point need
    // the lock to keep manifest writes and link-state mutations
    // coherent with a concurrent update.
    let _lock = match recovery_lock.take() {
        Some(lock) => lock,
        None => crate::state::StateLock::acquire(&roots.state_dir)?,
    };
    cancellation::check()?;

    // Re-read the manifest now that we hold the lock. The `orphans`
    // computed above is based on the caller's pre-lock snapshot; a
    // concurrent `shdeps update` may have added or removed entries
    // since then, which would invalidate the orphan list. Recomputing
    // inside the lock guarantees we mutate against the current state.
    //
    // The `config` is intentionally NOT re-read: shdeps does not write
    // to user config files, so a concurrent shdeps invocation cannot
    // mutate it. Only an operator-side edit (text editor, git pull)
    // would change it, and racing against that is out of scope —
    // the operator is expected to re-run prune if they edit config
    // mid-run.
    recover_fresh_repo_publications(roots, manifest_path)?;
    let initial_manifest = manifest::read(manifest_path)?;
    crate::update_transition::recover_pending_publications(
        config,
        &initial_manifest,
        manifest_path,
        roots,
    )?;
    cancellation::check()?;
    // Retry committed method transitions before orphan removal, mirroring the
    // update recovery order. Prune is a commit-only caller: `Installing`
    // journals whose target still matches are deferred untouched for a
    // same-installer update, while a removed or changed target and malformed
    // state fail closed before any hook, artifact cleanup, or manifest
    // mutation below.
    // Prune evaluates the full config (it does not platform-filter orphans),
    // matching the publications recovery above.
    let recovery_entries: Vec<Entry> = config.to_vec();
    let custom_fingerprints = recovery_entries
        .iter()
        .filter(|entry| entry.method == crate::method::CUSTOM)
        .map(|entry| {
            hooks
                .install_fingerprint(&entry.name, roots)
                .map(|fingerprint| (entry.name.clone(), fingerprint))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut recovered_items = Vec::new();
    let mut blocked: BTreeSet<String> = BTreeSet::new();
    for pending in crate::update_transition::recover_pending_transitions(
        &recovery_entries,
        &custom_fingerprints,
        manifest_path,
        roots,
        None,
    )? {
        let old = pending.old().clone();
        if let Some(detail) = finish_pending_transition_cleanup(pending, roots, &old)? {
            // Ambiguous cleanup can never complete exactly. Keep the new row
            // (when present) so the next run retries coherently instead of
            // stranding the journal with neither row matching, and surface the
            // failure while still pruning unrelated orphans.
            blocked.insert(old.name.clone());
            recovered_items.push(Item {
                entry: old,
                hook: Uninstall::MissingHook,
                cleanup: None,
                cleanup_error: Some(detail),
            });
        }
    }
    cancellation::check()?;
    let fresh_manifest = manifest::read(manifest_path)?;
    let orphans = self::orphans(&fresh_manifest, config);
    if orphans.is_empty() && recovered_items.is_empty() {
        return Ok(Summary {
            orphans,
            removed: Vec::new(),
            guarded_all_orphans: false,
            quiet_skipped: false,
        });
    }
    // Re-apply the all-orphans guard against the FRESH manifest. The
    // pre-lock check at the top of this function already decided
    // based on the caller's snapshot, but if the manifest changed
    // since then (a concurrent update added or removed deps), the
    // guard's "every tracked dep is about to be deleted" condition
    // can now be true even though it wasn't pre-lock. Without this
    // re-check the mutation loop would silently bulk-delete every
    // currently-tracked dep without the `--yes` confirmation.
    let fresh_unique_manifest: std::collections::BTreeSet<&str> = fresh_manifest
        .effective_entries()
        .into_iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let fresh_unique_orphans: std::collections::BTreeSet<&str> =
        orphans.iter().map(|entry| entry.name.as_str()).collect();
    let post_lock_prunes_everything = !fresh_unique_manifest.is_empty()
        && fresh_unique_orphans.len() == fresh_unique_manifest.len();
    if post_lock_prunes_everything && !options.yes {
        return Ok(Summary {
            orphans,
            removed: recovered_items,
            guarded_all_orphans: true,
            quiet_skipped: false,
        });
    }

    let cleanup_roots = cleanup_roots(roots);
    let mut removed = recovered_items;
    for entry in &orphans {
        cancellation::check()?;
        if blocked.contains(entry.name.as_str()) {
            continue;
        }
        cleanup::validate_manifest_artifact_entry(entry)?;
        let cleanup_evidence = capture_cleanup_evidence(entry, &cleanup_roots)?;
        cancellation::check()?;
        let mut hook = hooks.uninstall(&entry.name, roots)?;
        cancellation::check()?;
        if hook == Uninstall::SudoRequired {
            hook = if Process.run("sudo", &["true"], None)?.success {
                cancellation::check()?;
                match hooks.retry_uninstall(&entry.name, roots)? {
                    Uninstall::SudoRequired => Uninstall::Failed,
                    retried => retried,
                }
            } else {
                Uninstall::Failed
            };
            cancellation::check()?;
        }
        let preserve_regular_public =
            regular_public_claimed_by_survivor(entry, &fresh_manifest, config);
        // Cleanup plus manifest removal is one commit boundary. Honor a
        // latched signal before entering it, then let recovery invariants
        // complete rather than interrupting halfway through filesystem state.
        cancellation::check()?;
        let (cleanup, cleanup_error) = cleanup_orphan(
            entry,
            manifest_path,
            &cleanup_roots,
            preserve_regular_public,
            cleanup_evidence,
        )?;
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

fn capture_cleanup_evidence(
    entry: &ManifestEntry,
    roots: &cleanup::Roots,
) -> Result<cleanup::Evidence> {
    if entry.method != crate::method::GITHUB_REPO {
        return cleanup::capture_evidence(entry, roots);
    }
    let root = cleanup::safe_repo_root(entry, roots).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe repository identity in manifest: {}", entry.name),
        )
    })?;
    #[cfg(unix)]
    {
        crate::checkout_lock::with_checkout_lock_process_env(&root, |normalized| {
            crate::repo_transition::recover(normalized)?;
            cleanup::capture_evidence(entry, roots)
        })
    }
    #[cfg(not(unix))]
    {
        cleanup::capture_evidence(entry, roots)
    }
}

// Finishes one recovered post-commit cleanup under the old checkout lock,
// mirroring the update recovery path: lock, installer-transaction recovery,
// exact-evidence finish. Lock and recovery failures abort prune loudly; an
// ambiguous-cleanup warning is returned so the caller can contain it without
// deleting the new row out from under the retained journal.
fn finish_pending_transition_cleanup(
    pending: crate::update_transition::PendingCleanup,
    roots: &runtime::Roots,
    old: &ManifestEntry,
) -> Result<Option<String>> {
    if old.method != crate::method::GITHUB_REPO {
        return pending.finish(roots, None);
    }
    let cleanup_roots = cleanup_roots(roots);
    let Some(root) = cleanup::safe_repo_root(old, &cleanup_roots) else {
        return pending.finish(roots, None);
    };
    #[cfg(unix)]
    {
        crate::checkout_lock::with_checkout_lock_process_env(&root, |normalized| {
            if crate::repo_transition::recover(normalized)?.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "fresh repository ownership was not finalized before transition cleanup",
                )
                .into());
            }
            pending.finish(roots, Some(normalized))
        })
    }
    #[cfg(not(unix))]
    {
        pending.finish(roots, Some(&root))
    }
}

// Clean one orphan under its checkout lock only after arbitrary hooks have returned.
fn cleanup_orphan(
    entry: &ManifestEntry,
    manifest_path: &Path,
    cleanup_roots: &cleanup::Roots,
    preserve_regular_public: bool,
    cleanup_evidence: cleanup::Evidence,
) -> Result<(Option<cleanup::Summary>, Option<String>)> {
    cancellation::check()?;
    let captured_repo_root = (entry.method == crate::method::GITHUB_REPO)
        .then(|| {
            cleanup_evidence
                .managed_install_root()
                .map(Path::to_path_buf)
        })
        .flatten();
    let cleanup = |repo_root: Option<&Path>| -> Result<(Option<cleanup::Summary>, Option<String>)> {
        // A pending bin-link record is recovery authority, not ordinary
        // best-effort cleanup. Resolve or reject it before the legacy cleanup
        // wrapper converts filesystem failures into a warning and removes the
        // manifest row.
        let bin_state = crate::link_state::path(
            &cleanup_roots.state_dir,
            &entry.name,
            crate::link_state::Kind::Bin,
        );
        crate::link_state::recover_reconcile(&bin_state)?;
        let result = match cleanup::remove_builtin_with_evidence(
            entry,
            cleanup_roots,
            repo_root,
            preserve_regular_public,
            cleanup_evidence,
        ) {
            Ok(summary) => (Some(summary), None),
            Err(error) => (None, Some(error.to_string())),
        };

        // Bash removes tracking even when cleanup warns. Preserve that bias so
        // a partially missing filesystem state does not strand the manifest
        // forever. Lock-acquisition failures return before this point because
        // no checkout cleanup decision was safely made.
        manifest::remove(manifest_path, &entry.name)?;
        Ok(result)
    };

    if entry.method != crate::method::GITHUB_REPO {
        return cleanup(None);
    }
    let root = captured_repo_root.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe repository identity in manifest: {}", entry.name),
        )
    })?;
    #[cfg(unix)]
    {
        crate::checkout_lock::with_checkout_lock_process_env(&root, |normalized| {
            crate::repo_transition::recover(normalized)?;
            cleanup(Some(normalized))
        })
    }
    #[cfg(not(unix))]
    {
        cleanup(Some(&root))
    }
}

fn regular_public_claimed_by_survivor(
    orphan: &ManifestEntry,
    manifest: &Manifest,
    config: &[Entry],
) -> bool {
    if orphan.method != crate::method::GITHUB_RELEASE {
        return false;
    }
    let configured = config
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    manifest.effective_entries().into_iter().any(|installed| {
        installed.name != orphan.name
            && configured.contains(installed.name.as_str())
            && installed.cmd == orphan.cmd
            && (installed.method == crate::method::GITHUB_RELEASE
                || installed.method == crate::method::CUSTOM
                || crate::method::is_symlink_install_root(&installed.method))
    })
}

fn orphans(manifest: &Manifest, config: &[Entry]) -> Vec<ManifestEntry> {
    manifest.orphans(config)
}

#[cfg(unix)]
fn recover_fresh_repo_publications(roots: &runtime::Roots, manifest_path: &Path) -> Result<()> {
    for publication in
        crate::repo_transition::pending_fresh_publications(&roots.state_dir, &roots.install_dir)?
    {
        crate::checkout_lock::with_checkout_lock_process_env(
            &publication.checkout,
            |normalized| {
                match crate::repo_transition::recover(normalized)? {
                    Some(ownership) => {
                        if ownership != publication.ownership {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "fresh repository journal and durable index disagree",
                            )
                            .into());
                        }
                        crate::update_repo::recover_fresh_publication(
                            roots,
                            manifest_path,
                            normalized,
                            &ownership,
                        )?;
                    }
                    None => {
                        let manifest = manifest::read(manifest_path)?;
                        crate::repo_transition::finish_index_without_journal(
                            &roots.state_dir,
                            &publication,
                            &manifest,
                        )?;
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn recover_fresh_repo_publications(_roots: &runtime::Roots, _manifest_path: &Path) -> Result<()> {
    Ok(())
}

fn cleanup_roots(roots: &runtime::Roots) -> cleanup::Roots {
    cleanup::Roots {
        state_dir: roots.state_dir.clone(),
        install_dir: roots.install_dir.clone(),
        bin_dir: roots.bin_dir.clone(),
    }
}

impl fmt::Display for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.entry.name, self.entry.method)
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, run};
    use crate::config::{Entry, parse_entry};
    use crate::hooks::{BashCustomProbe, Uninstall};
    use crate::manifest::{self, ManifestEntry};
    use crate::runtime::Roots;
    use crate::update_transition;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    fn run_signal_boundary_subprocess(test_name: &str, child_env: &str) {
        crate::test_support::run_signal_boundary_subprocess(test_name, child_env);
    }

    #[test]
    #[cfg(unix)]
    fn prune_removes_orphan_manifest_and_owned_artifacts() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("remove-orphan");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let bin = fixture.roots.bin_dir.join("tool");
        let archive_bin = fixture.roots.install_dir.join("owner/tool/bin/tool");
        fixture.write(&archive_bin, "#!/bin/sh\n");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        symlink(&archive_bin, &bin).unwrap();
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
    #[cfg(unix)]
    fn prune_recovers_committed_fresh_repo_before_empty_manifest_fast_path() {
        let fixture = Fixture::new("prune-pending-fresh-repo");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let checkout = fixture.roots.install_dir.join("owner/tool");
        let staged = fixture.roots.install_dir.join("owner/tool.tmp.123");
        fixture.write(&staged.join("bin/tool"), "#!/bin/sh\n");
        let ownership = ManifestEntry::new(
            "owner/tool",
            "github:repo",
            "tool",
            checkout.display().to_string(),
        );
        crate::repo_transition::publish_fresh_directory(
            &checkout,
            &staged,
            ownership.clone(),
            &fixture.roots.state_dir,
        )
        .unwrap();
        // Model an uncatchable stop after manifest publication but before the
        // fresh-publication journal and its durable index were retired.
        manifest::upsert(&manifest_path, ownership).unwrap();

        let summary = run(
            &[],
            &manifest::Manifest::default(),
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(summary.removed.len(), 1);
        assert!(!checkout.exists());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
        assert!(
            crate::repo_transition::pending_fresh_publications(
                &fixture.roots.state_dir,
                &fixture.roots.install_dir,
            )
            .unwrap()
            .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_before_prune_commit_preserves_artifacts_and_manifest() {
        use std::os::unix::fs::symlink;

        const CHILD_ENV: &str = "SHDEPS_TEST_CANCEL_BEFORE_PRUNE_COMMIT";
        const TEST_NAME: &str =
            "prune::tests::cancellation_before_prune_commit_preserves_artifacts_and_manifest";
        if std::env::var_os(CHILD_ENV).is_none() {
            run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let signals = crate::cancellation::Signals::install().unwrap();
        let fixture = Fixture::new("cancel-before-prune-commit");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let install_root = fixture.roots.install_dir.join("owner/tool");
        let archive_bin = install_root.join("bin/tool");
        fixture.write(&archive_bin, "#!/bin/sh\n");
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        symlink(&archive_bin, &public).unwrap();
        let entry = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            public.display().to_string(),
        );
        manifest::upsert(&manifest_path, entry.clone()).unwrap();
        let cleanup_roots = super::cleanup_roots(&fixture.roots);
        let evidence = super::capture_cleanup_evidence(&entry, &cleanup_roots).unwrap();

        // Model an injected uninstall hook that returned success while
        // delivering TERM immediately before the prune commit boundary.
        // SAFETY: this subprocess installed the Shdeps signal owner above.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let result = super::cleanup_orphan(&entry, &manifest_path, &cleanup_roots, false, evidence);

        assert!(
            result.is_err(),
            "latched cancellation must reject prune commit"
        );
        assert!(
            archive_bin.exists(),
            "cancelled prune removed owned artifacts"
        );
        assert!(
            public.exists(),
            "cancelled prune removed its public command"
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_some(),
            "cancelled prune removed its manifest row"
        );
        assert_eq!(
            signals.finish_result(result.map(|_| 0)).unwrap(),
            128 + libc::SIGTERM
        );
    }

    #[test]
    fn prune_preserves_regular_command_owned_by_surviving_raw_release() {
        let fixture = Fixture::new("prune-raw-release-handoff");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "replacement release\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/replacement",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
            &[parse_entry("owner/replacement|github:repo|tool|-|-", None)],
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

        assert_eq!(fs::read_to_string(public).unwrap(), "replacement release\n");
        let remaining = manifest::read(&manifest_path).unwrap();
        assert!(remaining.get("owner/old").is_none());
        assert!(remaining.get("owner/replacement").is_some());
    }

    #[test]
    fn prune_preserves_regular_command_claimed_by_surviving_custom_provider() {
        let fixture = Fixture::new("prune-custom-command-handoff");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "custom replacement\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("replacement", "custom", "tool", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
            &[parse_entry("replacement|custom|tool|-|-", None)],
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

        assert_eq!(fs::read_to_string(public).unwrap(), "custom replacement\n");
    }

    #[test]
    fn prune_preserves_regular_command_claimed_by_surviving_symlink_provider() {
        for (case, survivor, config_line) in [
            (
                "repo",
                ManifestEntry::new(
                    "owner/replacement",
                    "github:repo",
                    "tool",
                    "/managed/owner/replacement",
                ),
                "owner/replacement|github:repo|tool|-|-",
            ),
            (
                "cargo",
                ManifestEntry::new("replacement", "cargo", "tool", "/managed/replacement"),
                "replacement|cargo|tool|-|-",
            ),
        ] {
            let fixture = Fixture::new(&format!("prune-symlink-provider-handoff-{case}"));
            let manifest_path = manifest::path(&fixture.roots.state_dir);
            let public = fixture.roots.bin_dir.join("tool");
            fixture.write(&public, "surviving adapter\n");
            manifest::upsert(
                &manifest_path,
                ManifestEntry::new(
                    "owner/old",
                    "github:release",
                    "tool",
                    public.display().to_string(),
                ),
            )
            .unwrap();
            manifest::upsert(&manifest_path, survivor).unwrap();
            let manifest = manifest::read(&manifest_path).unwrap();

            run(
                &[parse_entry(config_line, None)],
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

            assert_eq!(fs::read_to_string(public).unwrap(), "surviving adapter\n");
        }
    }

    #[test]
    fn prune_uses_only_effective_duplicate_manifest_row() {
        let fixture = Fixture::new("prune-effective-duplicate-row");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "preserve\n");
        fixture.write(
            &manifest_path,
            &format!(
                "orphan|github:release|tool|{}\n\
                 orphan|pkg|other|\n\
                 keep|custom|keep|\n",
                public.display()
            ),
        );
        let manifest = manifest::read(&manifest_path).unwrap();

        let summary = run(
            &[parse_entry("keep|custom|keep|-|-", None)],
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
        assert_eq!(summary.removed.len(), 1);
        assert_eq!(summary.removed[0].entry.method, "pkg");
        assert_eq!(fs::read_to_string(public).unwrap(), "preserve\n");
    }

    #[test]
    fn prune_does_not_treat_package_survivor_as_regular_command_owner() {
        let fixture = Fixture::new("prune-package-command-handoff");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "old raw release\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("replacement", "pkg", "tool", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
            &[parse_entry("replacement|pkg|tool|-|-", None)],
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

        assert!(fs::symlink_metadata(public).is_err());
    }

    #[test]
    fn prune_preserves_raw_command_replaced_by_uninstall_hook() {
        let fixture = Fixture::new("prune-hook-replaces-raw-command");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "old raw release\n");
        fixture.write(
            &fixture.roots.hooks_dir.join("owner/old.sh"),
            r#"uninstall() {
  printf 'replacement from hook\n' > "$SHDEPS_BIN_DIR/.tool.new"
  mv -f "$SHDEPS_BIN_DIR/.tool.new" "$SHDEPS_BIN_DIR/tool"
}
"#,
        );
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
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

        assert_eq!(summary.removed[0].hook, Uninstall::Removed);
        assert_eq!(
            fs::read_to_string(public).unwrap(),
            "replacement from hook\n"
        );
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn prune_preserves_raw_command_modified_in_place_by_uninstall_hook() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("prune-hook-modifies-raw-command-in-place");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "old\n");
        let timestamp = fixture.roots.state_dir.join("tool.original-time");
        fs::copy(&public, &timestamp).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&timestamp, fs::Permissions::from_mode(0o600)).unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("owner/old.sh"),
            r#"uninstall() {
  printf 'new\n' > "$SHDEPS_BIN_DIR/tool"
  touch -r "$SHDEPS_STATE_DIR/tool.original-time" "$SHDEPS_BIN_DIR/tool"
}
"#,
        );
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(fs::read_to_string(public).unwrap(), "new\n");
    }

    #[test]
    #[cfg(unix)]
    fn prune_removes_execute_only_raw_release_command() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("prune-execute-only-raw-command");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&public, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&public, fs::Permissions::from_mode(0o111)).unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert!(fs::symlink_metadata(public).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn prune_raw_release_preserves_unreadable_coincidental_install_root() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("prune-raw-unreadable-coincidental-root");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let coincidental = fixture.roots.install_dir.join("owner/old");
        fixture.write(&public, "old raw release\n");
        fixture.write(&coincidental.join("sentinel"), "preserve\n");
        fs::set_permissions(&coincidental, fs::Permissions::from_mode(0o000)).unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let result = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        );

        fs::set_permissions(&coincidental, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(coincidental.join("sentinel").is_file());
        assert_eq!(fs::read_to_string(public).unwrap(), "old raw release\n");
    }

    #[test]
    fn prune_raw_release_ignores_coincidental_non_directory_install_root() {
        let fixture = Fixture::new("prune-raw-coincidental-file-root");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let coincidental = fixture.roots.install_dir.join("owner/old");
        fixture.write(&public, "old raw release\n");
        fixture.write(&coincidental, "unrelated\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(fs::read_to_string(coincidental).unwrap(), "unrelated\n");
        assert!(fs::symlink_metadata(public).is_err());
    }

    #[test]
    fn prune_raw_release_ignores_coincidental_non_directory_ancestor() {
        let fixture = Fixture::new("prune-raw-coincidental-file-ancestor");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let coincidental = fixture.roots.install_dir.join("owner");
        fixture.write(&public, "old raw release\n");
        fixture.write(&coincidental, "unrelated\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(fs::read_to_string(coincidental).unwrap(), "unrelated\n");
        assert!(fs::symlink_metadata(public).is_err());
    }

    #[test]
    fn prune_preserves_ambiguous_legacy_release_launcher() {
        let fixture = Fixture::new("prune-ambiguous-release-launcher");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let legacy_root = fixture.roots.install_dir.join("owner/old");
        fixture.write(&public, "launcher\n");
        fixture.write(&legacy_root.join("payload"), "legacy archive\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(fs::read_to_string(public).unwrap(), "launcher\n");
        assert_eq!(
            fs::read_to_string(legacy_root.join("payload")).unwrap(),
            "legacy archive\n"
        );
    }

    #[test]
    fn prune_rejects_corrupt_explicit_release_marker_before_retiring_state() {
        let fixture = Fixture::new("prune-corrupt-release-marker");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let public = fixture.roots.bin_dir.join("tool");
        let root = fixture.roots.install_dir.join("owner/old");
        fixture.write(&public, "launcher\n");
        fixture.write(&root.join(".shdeps-release-layout"), "unknown\n");
        let bin_state = crate::link_state::path(
            &fixture.roots.state_dir,
            "owner/old",
            crate::link_state::Kind::Bin,
        );
        crate::link_state::write(&bin_state, std::slice::from_ref(&public)).unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/old",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let error = run(
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
        .unwrap_err();

        assert!(error.to_string().contains("unknown release archive marker"));
        assert_eq!(fs::read_to_string(public).unwrap(), "launcher\n");
        assert!(root.is_dir());
        assert!(bin_state.is_file());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/old")
                .is_some()
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_uses_install_root_captured_before_uninstall_hook_retargets_base() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("prune-hook-retargets-install-base");
        let physical_a = fixture.roots.home.join("physical-a");
        let physical_b = fixture.roots.home.join("physical-b");
        fs::create_dir_all(physical_a.join("tool")).unwrap();
        fs::write(physical_a.join("tool/old"), "remove\n").unwrap();
        fs::create_dir_all(physical_b.join("tool")).unwrap();
        fs::write(physical_b.join("tool/replacement"), "preserve\n").unwrap();
        symlink(&physical_a, &fixture.roots.install_dir).unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            r#"uninstall() {
  rm -f "$SHDEPS_INSTALL_DIR"
  ln -s "$SHDEPS_STATE_DIR/../physical-b" "$SHDEPS_INSTALL_DIR"
}
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "cargo",
                "tool",
                physical_a.join("tool/bin/tool").display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert!(!physical_a.join("tool").exists());
        assert_eq!(
            fs::read_to_string(physical_b.join("tool/replacement")).unwrap(),
            "preserve\n"
        );
        assert_eq!(
            fs::canonicalize(&fixture.roots.install_dir).unwrap(),
            physical_b
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_preserves_external_root_recreated_by_uninstall_hook() {
        let fixture = Fixture::new("prune-hook-recreates-external-root");
        let root = fixture.roots.install_dir.join("tool");
        fixture.write(&root.join("old"), "old\n");
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            r#"uninstall() {
  rm -rf "$SHDEPS_INSTALL_DIR/tool"
  mkdir -p "$SHDEPS_INSTALL_DIR/tool"
  printf 'replacement\n' > "$SHDEPS_INSTALL_DIR/tool/replacement"
}
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "cargo",
                "tool",
                root.join("bin/tool").display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(
            fs::read_to_string(root.join("replacement")).unwrap(),
            "replacement\n"
        );
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn prune_preserves_external_root_modified_in_place_by_uninstall_hook() {
        let fixture = Fixture::new("prune-hook-modifies-external-root-in-place");
        let root = fixture.roots.install_dir.join("tool");
        let payload = root.join("deep/payload");
        let timestamp = fixture.roots.state_dir.join("payload.original-time");
        fixture.write(&payload, "old\n");
        fs::copy(&payload, &timestamp).unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            r#"uninstall() {
  printf 'new\n' > "$SHDEPS_INSTALL_DIR/tool/deep/payload"
  touch -r "$SHDEPS_STATE_DIR/payload.original-time" "$SHDEPS_INSTALL_DIR/tool/deep/payload"
}
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "cargo",
                "tool",
                root.join("bin/tool").display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(fs::read_to_string(payload).unwrap(), "new\n");
    }

    #[test]
    #[cfg(unix)]
    fn prune_preserves_repo_root_recreated_by_uninstall_hook() {
        let fixture = Fixture::new("prune-hook-recreates-repo-root");
        let root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&root.join("old"), "old\n");
        fixture.write(
            &fixture.roots.hooks_dir.join("owner/tool.sh"),
            r#"uninstall() {
  rm -rf "$SHDEPS_INSTALL_DIR/owner/tool"
  mkdir -p "$SHDEPS_INSTALL_DIR/owner/tool"
  printf 'replacement\n' > "$SHDEPS_INSTALL_DIR/owner/tool/replacement"
}
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                root.display().to_string(),
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

        assert_eq!(
            fs::read_to_string(root.join("replacement")).unwrap(),
            "replacement\n"
        );
        assert!(summary.removed[0].cleanup_error.is_some());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn prune_accepts_repo_root_removed_by_uninstall_hook() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("prune-hook-removes-repo-root");
        let root = fixture.roots.install_dir.join("owner/tool");
        let target = root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&target, "old\n");
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&target, &public).unwrap();
        let bin_state = crate::link_state::path(
            &fixture.roots.state_dir,
            "owner/tool",
            crate::link_state::Kind::Bin,
        );
        crate::link_state::write(&bin_state, std::slice::from_ref(&public)).unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("owner/tool.sh"),
            "uninstall() { rm -rf \"$SHDEPS_INSTALL_DIR/owner/tool\"; }\n",
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                root.display().to_string(),
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

        assert!(summary.removed[0].cleanup_error.is_none());
        assert!(fs::symlink_metadata(public).is_err());
        assert!(fs::symlink_metadata(bin_state).is_err());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn prune_does_not_follow_install_base_created_by_uninstall_hook() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("prune-hook-creates-install-base-link");
        let foreign = fixture.roots.state_dir.parent().unwrap().join("foreign");
        let logical_target = fixture.roots.install_dir.join("tool/bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&logical_target, &public).unwrap();
        crate::link_state::write(
            &crate::link_state::path(
                &fixture.roots.state_dir,
                "tool",
                crate::link_state::Kind::Bin,
            ),
            std::slice::from_ref(&public),
        )
        .unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            r#"uninstall() {
  mkdir -p "$SHDEPS_STATE_DIR/../foreign/tool/bin"
  printf 'replacement\n' > "$SHDEPS_STATE_DIR/../foreign/tool/bin/tool"
  ln -s "$SHDEPS_STATE_DIR/../foreign" "$SHDEPS_INSTALL_DIR"
}
"#,
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "cargo",
                "tool",
                logical_target.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert_eq!(
            fs::read_to_string(foreign.join("tool/bin/tool")).unwrap(),
            "replacement\n"
        );
        assert_eq!(fs::read_link(&public).unwrap(), logical_target);
    }

    #[test]
    #[cfg(unix)]
    fn prune_removes_tracked_dangling_external_link_when_root_is_absent() {
        use std::os::unix::fs::symlink;

        for case in ["missing-root", "missing-base", "broken-base"] {
            let fixture = Fixture::new(&format!("prune-dangling-external-{case}"));
            let logical_target = fixture.roots.install_dir.join("tool/bin/tool");
            let public = fixture.roots.bin_dir.join("tool");
            match case {
                "missing-root" => fs::create_dir_all(&fixture.roots.install_dir).unwrap(),
                "broken-base" => {
                    symlink(
                        fixture.roots.home.join("missing-target"),
                        &fixture.roots.install_dir,
                    )
                    .unwrap();
                }
                _ => {}
            }
            fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
            symlink(&logical_target, &public).unwrap();
            let bin_state = crate::link_state::path(
                &fixture.roots.state_dir,
                "tool",
                crate::link_state::Kind::Bin,
            );
            crate::link_state::write(&bin_state, std::slice::from_ref(&public)).unwrap();
            let manifest_path = manifest::path(&fixture.roots.state_dir);
            manifest::upsert(
                &manifest_path,
                ManifestEntry::new(
                    "tool",
                    "cargo",
                    "tool",
                    logical_target.display().to_string(),
                ),
            )
            .unwrap();
            let manifest = manifest::read(&manifest_path).unwrap();

            run(
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

            assert!(fs::symlink_metadata(public).is_err(), "case: {case}");
            assert!(fs::symlink_metadata(bin_state).is_err(), "case: {case}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn prune_removes_dangling_logical_link_when_install_alias_disappears() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("prune-hook-removes-install-base-link");
        let physical = fixture.roots.home.join("physical");
        let root = physical.join("tool");
        let logical_target = fixture.roots.install_dir.join("tool/bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&root.join("bin/tool"), "old\n");
        symlink(&physical, &fixture.roots.install_dir).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&logical_target, &public).unwrap();
        crate::link_state::write(
            &crate::link_state::path(
                &fixture.roots.state_dir,
                "tool",
                crate::link_state::Kind::Bin,
            ),
            std::slice::from_ref(&public),
        )
        .unwrap();
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            "uninstall() { rm -f \"$SHDEPS_INSTALL_DIR\"; }\n",
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "tool",
                "cargo",
                "tool",
                logical_target.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        run(
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

        assert!(fs::symlink_metadata(public).is_err());
        assert!(!root.exists());
        assert!(fs::symlink_metadata(&fixture.roots.install_dir).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn prune_normalizes_legacy_repo_manifest_path_before_locking() {
        let fixture = Fixture::new("prune-repo-legacy-curdir");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&install_root.join("artifact"), "managed\n");
        let legacy_spelling = format!("{}/./owner/tool", fixture.roots.install_dir.display());
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("owner/tool", "github:repo", "tool", legacy_spelling),
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

        assert!(!install_root.exists());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
        assert!(summary.removed[0].cleanup_error.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn prune_refuses_cleanup_when_checkout_lock_is_malformed() {
        let fixture = Fixture::new("prune-repo-lock-structural-wiring");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&install_root.join("artifact"), "managed\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let malformed_lock = install_root.parent().unwrap().join(".tool.install.lock");
        fs::write(&malformed_lock, "foreign\n").unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let result = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        );

        assert!(result.is_err());
        assert!(install_root.join("artifact").exists());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_some()
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_recovers_interrupted_repo_transition_before_cleanup() {
        use std::os::unix::fs::MetadataExt;

        let fixture = Fixture::new("prune-repo-transition-recovery");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&install_root.join("artifact"), "managed\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let metadata = fs::symlink_metadata(&install_root).unwrap();
        let journal = install_root
            .parent()
            .unwrap()
            .join(".tool.shdeps-repo-transition-v1");
        fs::create_dir_all(&journal).unwrap();
        let record = serde_json::json!({
            "format": "shdeps repository transition v1",
            "checkout": install_root.clone(),
            "previous": {
                "kind": "directory",
                "device": metadata.dev(),
                "inode": metadata.ino(),
                "target": null
            },
            "desired": {
                "Symlink": fixture.roots.git_dev_dir.join("tool")
            }
        });
        fs::write(
            journal.join("record"),
            format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
        )
        .unwrap();
        fs::rename(&install_root, journal.join("previous")).unwrap();
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

        assert!(summary.removed[0].cleanup_error.is_none());
        assert!(!install_root.exists());
        assert!(!journal.exists());
        assert!(manifest::read(&manifest_path).unwrap().entries().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn prune_rejects_unrecovered_checkout_installer_transaction() {
        let fixture = Fixture::new("prune-installer-transaction");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&install_root.join("artifact"), "managed\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let installer_transaction = install_root
            .parent()
            .unwrap()
            .join(".tool.install.transaction");
        fs::create_dir_all(&installer_transaction).unwrap();
        fs::write(installer_transaction.join("identity.partial"), "preserve\n").unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let error = run(
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
        .unwrap_err();

        assert!(error.to_string().contains("rerun the checkout installer"));
        assert!(install_root.join("artifact").exists());
        assert!(installer_transaction.is_dir());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_some()
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_rejects_malformed_link_recovery_before_removing_manifest() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("prune-malformed-link-recovery");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let install_root = fixture.roots.install_dir.join("owner/tool");
        let source = install_root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write(&source, "#!/bin/sh\n");
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        symlink(&source, &public).unwrap();
        let bin_state = crate::link_state::path(
            &fixture.roots.state_dir,
            "owner/tool",
            crate::link_state::Kind::Bin,
        );
        crate::link_state::write(&bin_state, std::slice::from_ref(&public)).unwrap();
        let transaction = bin_state.with_file_name("tool.binlinks.reconcile-v1");
        fs::write(&transaction, "not json\n").unwrap();
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let error = run(
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
        .unwrap_err();

        assert!(error.to_string().contains("malformed link reconciliation"));
        assert!(install_root.is_dir());
        assert_eq!(fs::read_link(&public).unwrap(), source);
        assert!(transaction.is_file());
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_some()
        );
    }

    #[test]
    fn prune_rejects_unsafe_manifest_name_before_hooks_or_cleanup() {
        let fixture = Fixture::new("prune-unsafe-manifest-name");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let outside = fixture.roots.install_dir.parent().unwrap().join("outside");
        fixture.write(&outside.join("sentinel"), "preserve\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("../outside", "github:repo", "outside", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let result = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        );

        let error = result.expect_err("unsafe manifest name was accepted for pruning");
        assert!(error.to_string().contains("unsafe dependency name"));
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("../outside")
                .is_some(),
            "malformed state must remain available for manual recovery"
        );
    }

    #[test]
    fn prune_rejects_unsafe_manifest_command_before_cleanup() {
        let fixture = Fixture::new("prune-unsafe-manifest-command");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let outside = fixture
            .roots
            .bin_dir
            .parent()
            .unwrap()
            .join("outside-command");
        fixture.write(&outside, "preserve\n");
        manifest::upsert(
            &manifest_path,
            ManifestEntry::new("owner/tool", "github:release", "../outside-command", ""),
        )
        .unwrap();
        let manifest = manifest::read(&manifest_path).unwrap();

        let result = run(
            &[],
            &manifest,
            &manifest_path,
            &fixture.roots,
            &fixture.hooks,
            Options {
                yes: true,
                ..Options::default()
            },
        );

        let error = result.expect_err("unsafe manifest command was accepted for pruning");
        assert!(error.to_string().contains("unsafe command name"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "preserve\n");
        assert!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .is_some(),
            "malformed state must remain available for manual recovery"
        );
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

    /// Starts a cargo-to-pkg method journal bound to the apt installer.
    #[cfg(unix)]
    fn begin_pkg_journal(
        fixture: &Fixture,
        manifest_path: &Path,
        entry: &Entry,
    ) -> update_transition::DurableTransition {
        let manifest = manifest::read(manifest_path).unwrap();
        let transition =
            update_transition::by_name(&manifest, std::slice::from_ref(entry), &fixture.roots)
                .unwrap()
                .remove(entry.name.as_str())
                .unwrap();
        let apt = update_transition::PkgInstallerIdentity::for_target(
            &entry.name,
            &entry.aliases,
            "apt",
            false,
        );
        update_transition::begin_durable_transition(
            entry,
            Some(&transition),
            &fixture.roots,
            manifest_path,
            Some(apt),
        )
        .unwrap()
        .expect("cargo-to-pkg starts a durable transition")
    }

    /// Starts a method journal targeting the custom provider with a fixed hook
    /// fingerprint.
    #[cfg(unix)]
    fn begin_custom_journal(
        fixture: &Fixture,
        manifest_path: &Path,
        entry: &Entry,
    ) -> update_transition::DurableTransition {
        let manifest = manifest::read(manifest_path).unwrap();
        let transition =
            update_transition::by_name(&manifest, std::slice::from_ref(entry), &fixture.roots)
                .unwrap()
                .remove(entry.name.as_str())
                .unwrap();
        update_transition::begin_custom_durable_transition(
            entry,
            Some(&transition),
            &fixture.roots,
            manifest_path,
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        )
        .unwrap()
        .expect("starts a durable custom transition")
    }

    /// Drives a custom journal to `ManifestCommitted`, simulating a crash
    /// after the manifest swap but before old cleanup.
    #[cfg(unix)]
    fn commit_custom_journal(fixture: &Fixture, manifest_path: &Path, entry: &Entry) {
        let mut durable = begin_custom_journal(fixture, manifest_path, entry);
        durable.mark_installing(&fixture.roots).unwrap();
        manifest::upsert(
            durable.manifest_path(),
            ManifestEntry::new(entry.name.clone(), "custom", entry.cmd.clone(), ""),
        )
        .unwrap();
        durable.mark_installed(entry, &fixture.roots).unwrap();
        durable
            .commit(entry, &fixture.roots, manifest_path)
            .unwrap();
    }

    /// Sets up an old cargo provider with owned artifacts and manifest row.
    #[cfg(unix)]
    fn write_old_cargo_provider(fixture: &Fixture) -> (PathBuf, ManifestEntry) {
        let old_root = fixture.roots.install_dir.join("tool");
        fixture.write(&old_root.join("bin/tool"), "#!/bin/sh\n");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new("tool", "cargo", "tool", old_root.display().to_string());
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        (manifest_path, old)
    }

    /// Sets up an old github:repo provider with owned checkout and manifest row.
    #[cfg(unix)]
    fn write_old_repo_provider(fixture: &Fixture) -> (PathBuf, ManifestEntry) {
        let old_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&old_root.join("bin/tool"), "#!/bin/sh\n");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:repo",
            "tool",
            old_root.display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        (manifest_path, old)
    }

    /// Sets up an old github:release archive provider with proven marker and
    /// manifest row.
    #[cfg(unix)]
    fn write_old_release_provider(fixture: &Fixture) -> (PathBuf, ManifestEntry) {
        let old_root = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&old_root.join("bin/tool"), "#!/bin/sh\n");
        fixture.write(
            &crate::github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/tool",
            ),
            "v1 archive\n",
        );
        let manifest_path = manifest::path(&fixture.roots.state_dir);
        let old = ManifestEntry::new(
            "owner/tool",
            "github:release",
            "tool",
            old_root.display().to_string(),
        );
        manifest::upsert(&manifest_path, old.clone()).unwrap();
        (manifest_path, old)
    }

    #[test]
    #[cfg(unix)]
    fn prune_retries_committed_transition_cleanup_with_no_orphans() {
        let fixture = Fixture::new("transition-cleanup-no-orphans");
        let (manifest_path, _old) = write_old_cargo_provider(&fixture);
        let entry = parse_entry("tool|pkg|tool|-|-", Some("apt"));
        let mut durable = begin_pkg_journal(&fixture, &manifest_path, &entry);
        durable.mark_installing(&fixture.roots).unwrap();
        manifest::upsert(
            durable.manifest_path(),
            ManifestEntry::new("tool", "pkg", "tool", ""),
        )
        .unwrap();
        durable.mark_installed(&entry, &fixture.roots).unwrap();
        durable
            .commit(&entry, &fixture.roots, &manifest_path)
            .unwrap();
        drop(durable);

        let manifest = manifest::read(&manifest_path).unwrap();
        assert!(manifest.orphans(std::slice::from_ref(&entry)).is_empty());
        let summary = run(
            std::slice::from_ref(&entry),
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

        assert!(!summary.has_errors(), "{summary:?}");
        assert!(summary.removed.is_empty());
        assert!(
            !fixture.roots.install_dir.join("tool").exists(),
            "prune must retry post-swap cleanup even with no orphans"
        );
        assert!(
            !fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .exists(),
            "acknowledged cleanup must retire its transition journal"
        );
        assert_eq!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("tool")
                .unwrap()
                .method,
            "pkg"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_surfaces_persistent_committed_transition_cleanup_failure() {
        let fixture = Fixture::new("transition-cleanup-failure");
        let checkout = fixture.roots.install_dir.join("owner/tool");
        fixture.write(&checkout.join("bin/tool"), "#!/bin/sh\n");
        let manifest_path = manifest::path(&fixture.roots.state_dir);
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
        let entry = parse_entry("owner/tool|pkg|tool|-|-", Some("apt"));
        let manifest = manifest::read(&manifest_path).unwrap();
        let transition =
            update_transition::by_name(&manifest, std::slice::from_ref(&entry), &fixture.roots)
                .unwrap()
                .remove(entry.name.as_str())
                .unwrap();
        let apt = update_transition::PkgInstallerIdentity::for_target(
            &entry.name,
            &entry.aliases,
            "apt",
            false,
        );
        let mut durable = update_transition::begin_durable_transition(
            &entry,
            Some(&transition),
            &fixture.roots,
            &manifest_path,
            Some(apt),
        )
        .unwrap()
        .unwrap();
        durable.mark_installing(&fixture.roots).unwrap();
        manifest::upsert(
            durable.manifest_path(),
            ManifestEntry::new("owner/tool", "pkg", "tool", ""),
        )
        .unwrap();
        durable.mark_installed(&entry, &fixture.roots).unwrap();
        durable
            .commit(&entry, &fixture.roots, &manifest_path)
            .unwrap();
        drop(durable);
        // Modify the old checkout after the snapshot so repository cleanup
        // disagrees persistently (a hard failure, not a silent preserve).
        fixture.write(&checkout.join("intruder"), "x\n");

        let manifest = manifest::read(&manifest_path).unwrap();
        let summary = run(
            std::slice::from_ref(&entry),
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

        assert!(summary.has_errors(), "{summary:?}");
        assert_eq!(summary.removed.len(), 1);
        assert!(summary.removed[0].cleanup_error.is_some());
        assert!(
            checkout.exists(),
            "ambiguous old artifacts must be preserved"
        );
        assert!(
            fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .is_dir(),
            "failed cleanup must retain its journal for retry"
        );
        assert_eq!(
            manifest::read(&manifest_path)
                .unwrap()
                .get("owner/tool")
                .unwrap()
                .method,
            "pkg"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_finishes_committed_cleanup_then_prunes_removed_provider() {
        let fixture = Fixture::new("transition-cleanup-removed");
        let (manifest_path, _old) = write_old_cargo_provider(&fixture);
        let entry = parse_entry("tool|pkg|tool|-|-", Some("apt"));
        let mut durable = begin_pkg_journal(&fixture, &manifest_path, &entry);
        durable.mark_installing(&fixture.roots).unwrap();
        manifest::upsert(
            durable.manifest_path(),
            ManifestEntry::new("tool", "pkg", "tool", ""),
        )
        .unwrap();
        durable.mark_installed(&entry, &fixture.roots).unwrap();
        durable
            .commit(&entry, &fixture.roots, &manifest_path)
            .unwrap();
        drop(durable);

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

        assert!(!summary.has_errors(), "{summary:?}");
        assert!(
            !fixture.roots.install_dir.join("tool").exists(),
            "prune must finish old cleanup before pruning the new provider"
        );
        assert!(
            manifest::read(&manifest_path).unwrap().entries().is_empty(),
            "the removed new provider must be pruned"
        );
        assert!(
            !fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .exists(),
            "no journal may remain to wedge a future update"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_finishes_cargo_to_custom_cleanup_then_prunes_removed_provider() {
        let fixture = Fixture::new("transition-cleanup-custom-removed");
        let (manifest_path, _old) = write_old_cargo_provider(&fixture);
        let entry = parse_entry("tool|custom|tool|-|-", None);
        commit_custom_journal(&fixture, &manifest_path, &entry);

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

        assert!(!summary.has_errors(), "{summary:?}");
        assert!(
            !fixture.roots.install_dir.join("tool").exists(),
            "prune must finish old cleanup before pruning the new provider"
        );
        assert!(
            manifest::read(&manifest_path).unwrap().entries().is_empty(),
            "the removed new provider must be pruned"
        );
        assert!(
            !fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .exists(),
            "no journal may remain to wedge a future update"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_finishes_repo_to_custom_cleanup_then_prunes_removed_provider() {
        let fixture = Fixture::new("transition-cleanup-repo-custom-removed");
        let (manifest_path, _old) = write_old_repo_provider(&fixture);
        let entry = parse_entry("owner/tool|custom|tool|-|-", None);
        commit_custom_journal(&fixture, &manifest_path, &entry);

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

        assert!(!summary.has_errors(), "{summary:?}");
        assert!(
            !fixture.roots.install_dir.join("owner/tool").exists(),
            "prune must finish old checkout cleanup before pruning the new provider"
        );
        assert!(
            manifest::read(&manifest_path).unwrap().entries().is_empty(),
            "the removed new provider must be pruned"
        );
        assert!(
            !fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .exists(),
            "no journal may remain to wedge a future update"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_finishes_release_to_custom_cleanup_then_prunes_removed_provider() {
        let fixture = Fixture::new("transition-cleanup-release-custom-removed");
        let (manifest_path, _old) = write_old_release_provider(&fixture);
        let entry = parse_entry("owner/tool|custom|tool|-|-", None);
        commit_custom_journal(&fixture, &manifest_path, &entry);

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

        assert!(!summary.has_errors(), "{summary:?}");
        assert!(
            !fixture.roots.install_dir.join("owner/tool").exists(),
            "prune must finish old archive cleanup before pruning the new provider"
        );
        assert!(
            manifest::read(&manifest_path).unwrap().entries().is_empty(),
            "the removed new provider must be pruned"
        );
        assert!(
            !fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .exists(),
            "no journal may remain to wedge a future update"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_refuses_installing_transition_with_removed_config_before_any_mutation() {
        let fixture = Fixture::new("transition-installing-removed");
        fixture.write(
            &fixture.roots.hooks_dir.join("tool.sh"),
            "uninstall() { printf '%s\\n' \"$1\" > \"$SHDEPS_STATE_DIR/hook-ran\"; }\n",
        );
        let (manifest_path, old) = write_old_cargo_provider(&fixture);
        let entry = parse_entry("tool|pkg|tool|-|-", Some("apt"));
        let mut durable = begin_pkg_journal(&fixture, &manifest_path, &entry);
        durable.mark_installing(&fixture.roots).unwrap();
        drop(durable);

        let manifest = manifest::read(&manifest_path).unwrap();
        let error = run(
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
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("no longer matches configuration"),
            "unexpected error: {error}"
        );
        assert!(
            message.contains("remove the stale transition record"),
            "wedge error must name the escape remedy: {error}"
        );
        assert!(
            !fixture.roots.state_dir.join("hook-ran").exists(),
            "no uninstall hook may run before the fail-closed check"
        );
        assert!(
            fixture.roots.install_dir.join("tool/bin/tool").exists(),
            "old ownership needed for safe retry must survive"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&old)
        );
        assert!(
            fixture
                .roots
                .state_dir
                .join(".method-transitions-v1")
                .is_dir(),
            "the ambiguous journal must be retained for retry"
        );
    }

    #[test]
    #[cfg(unix)]
    fn prune_rejects_malformed_transition_state_before_mutating() {
        let fixture = Fixture::new("transition-malformed");
        let (manifest_path, old) = write_old_cargo_provider(&fixture);
        let transition_dir = fixture.roots.state_dir.join(".method-transitions-v1");
        fs::create_dir_all(&transition_dir).unwrap();
        fs::write(
            transition_dir.join(format!("{}.json", "0".repeat(64))),
            "{invalid",
        )
        .unwrap();

        let manifest = manifest::read(&manifest_path).unwrap();
        let error = run(
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
        .unwrap_err();
        assert!(
            !error.to_string().is_empty(),
            "malformed transition state must fail prune"
        );
        assert!(
            fixture.roots.install_dir.join("tool/bin/tool").exists(),
            "no artifact cleanup may run before transition validation"
        );
        assert_eq!(
            manifest::read(&manifest_path).unwrap().get("tool"),
            Some(&old)
        );
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
        crate::test_support::temp_dir(&format!("shdeps-prune-{name}"))
    }
}
