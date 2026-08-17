//! Method-transition helpers for `shdeps update`.
//!
//! Update methods write new artifacts into the same public paths and state
//! files that old methods used. This module snapshots old ownership before the
//! install starts, then cleans only that snapshot after the new method succeeds.
//! Keeping the transaction-sensitive code here prevents each installer from
//! learning a partial version of the same migration rules.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Result;
use crate::cleanup;
use crate::config::{self, Entry};
use crate::github_release_install::{self, ArchiveState};
use crate::link_state::{self, Kind};
use crate::manifest::{self, Manifest, ManifestEntry};
use crate::method;
use crate::platform::{self, RuntimeEnv};
use crate::runtime::Roots;
use crate::update::Item;

static MANIFEST_STAGE_NONCE: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
static PUBLIC_TRANSITION_NONCE: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
const PUBLIC_TRANSITION_FORMAT: &str = "shdeps public command transition v1";
#[cfg(unix)]
const MAX_PUBLIC_TRANSITION_RECORD_BYTES: u64 = 64 * 1024;

/// Pre-install snapshot for a configured method transition.
#[derive(Debug, Clone)]
pub(crate) struct Transition {
    old: ManifestEntry,
    bin_links: Vec<PathBuf>,
    extra_links: Vec<PathBuf>,
    archive_state: ArchiveState,
    archive_root_identity: Option<FileIdentity>,
    cleanup_evidence: cleanup::Evidence,
}

#[cfg(unix)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicTransitionRecord {
    format: String,
    public: PathBuf,
    swap: PathBuf,
    source: PathBuf,
    old: ManifestEntry,
    new: ManifestEntry,
    expected: cleanup::FileIdentity,
}

#[derive(Debug)]
enum PublicPublication {
    None,
    Warning(String),
    #[cfg(unix)]
    Pending(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// Rejects an implicit dependency rename that reuses an installed command.
///
/// Method transitions are transactional only when the logical dependency name
/// stays stable. A differently named config entry can otherwise mistake the
/// old command for proof that its new provider is installed, while later prune
/// treats the old manifest row as unrelated. Require the operator to prune the
/// old identity first instead of guessing replacement intent from a command
/// collision alone.
pub(crate) fn reject_identity_handoffs(
    manifest: &Manifest,
    entries: &[Entry],
    env: &RuntimeEnv,
    pkg_mgr: &str,
) -> Result<()> {
    let active_entries = entries
        .iter()
        .filter(|entry| {
            matches!(
                platform::filter_match(&entry.filter, env),
                platform::FilterMatch::Match
            ) && !(entry.method == method::PKG
                && config::resolve_override_for_runtime(
                    &entry.name,
                    &entry.aliases,
                    Some(pkg_mgr),
                    env.is_android(),
                ) == "NONE")
        })
        .collect::<Vec<_>>();
    let mut command_claims = HashMap::<&str, &str>::new();
    for entry in &active_entries {
        if entry.cmd.is_empty() {
            continue;
        }
        if let Some(previous) = command_claims.insert(&entry.cmd, &entry.name) {
            if previous != entry.name {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "duplicate active command claim for `{}`: configured `{previous}` conflicts with `{}`; give each dependency a distinct command",
                        entry.cmd, entry.name
                    ),
                )
                .into());
            }
        }
    }

    for old in manifest.effective_entries() {
        if old.cmd.is_empty() {
            continue;
        }
        if let Some(new) = active_entries
            .iter()
            .copied()
            .find(|entry| entry.name != old.name && entry.cmd == old.cmd)
        {
            // A prior Shdeps version may already have completed and recorded
            // this provider change while retaining a configured-but-inactive
            // platform row (or the old command spelling for another active
            // dependency). In that state there is no rename left to infer:
            // the manifest already names the sole active owner of the exact
            // command. Let the normal method-specific ownership checks verify
            // it and let same-name updates refresh any stale old row.
            //
            // Do not extend this to name-only or method-only matches. Without
            // the exact recorded command, a newly configured provider could
            // still mistake the old provider's executable for its own proof.
            if configured_identity_relinquished_command(old, entries, env, pkg_mgr)
                && manifest
                    .get(&new.name)
                    .is_some_and(|installed| installed.cmd == new.cmd)
            {
                continue;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "unsupported dependency identity handoff for command `{}`: manifest `{}` conflicts with configured `{}`; remove the old declaration, run `shdeps prune`, then retry the replacement",
                    old.cmd, old.name, new.name
                ),
            )
            .into());
        }
    }

    Ok(())
}

/// Builds a transition map keyed by dependency name.
pub(crate) fn by_name(
    manifest: &Manifest,
    entries: &[Entry],
    roots: &Roots,
) -> Result<HashMap<String, Transition>> {
    cleanup::method_transitions(manifest, entries)
        .into_iter()
        .map(|entry| {
            // A saved row is human-editable state. Validate it before its name
            // or command participates in link-state and public-bin paths.
            cleanup::validate_manifest_artifact_entry(&entry)?;
            let bin_state = link_state::path(&roots.state_dir, &entry.name, Kind::Bin);
            // A method change may be the first operation after a killed repo
            // relink. Recover prepublication command ownership before taking
            // the old-method snapshot so cleanup cannot strand a newly live
            // symlink merely because the repo disappeared from configuration.
            link_state::recover_reconcile(&bin_state)?;
            let bin_links = link_state::read(&bin_state)?;
            let extra_links = link_state::read(&link_state::path(
                &roots.state_dir,
                &entry.name,
                Kind::Extras,
            ))?;
            let archive_state = if entry.method == method::GITHUB_RELEASE {
                github_release_install::archive_state(
                    &roots.state_dir,
                    &roots.install_dir,
                    &roots.bin_dir.join(&entry.cmd),
                    &entry.name,
                )?
            } else {
                ArchiveState::None
            };
            let archive_root_identity = if entry.method == method::GITHUB_RELEASE {
                file_identity(&roots.install_dir.join(&entry.name))?
            } else {
                None
            };
            let cleanup_evidence = cleanup::capture_evidence(
                &entry,
                &cleanup::Roots {
                    state_dir: roots.state_dir.clone(),
                    install_dir: roots.install_dir.clone(),
                    bin_dir: roots.bin_dir.clone(),
                },
            )?;
            Ok((
                entry.name.clone(),
                Transition {
                    old: entry,
                    bin_links,
                    extra_links,
                    archive_state,
                    archive_root_identity,
                    cleanup_evidence,
                },
            ))
        })
        .collect()
}

/// Returns the old manifest row for a transition.
pub(crate) fn old(transition: &Transition) -> &ManifestEntry {
    &transition.old
}

/// Returns whether the previous built-in method proves ownership of the
/// canonical install root that a new `github:repo` install will reuse.
///
/// External toolchains always install beneath that managed root. A GitHub
/// release owns it only when archive evidence is proven; raw release binaries,
/// packages, and custom hooks do not authorize deleting a coincidental path.
pub(crate) fn owns_repo_destination(transition: &Transition) -> bool {
    method::is_external(&transition.old.method)
        || (transition.old.method == method::GITHUB_RELEASE
            && transition.archive_state == ArchiveState::Proven)
}

/// Refreshes filesystem-derived transition evidence under the checkout lock.
///
/// `by_name` runs before workers acquire dependency checkout locks. Structural
/// state in the manifest remains valid across that wait, but an installer may
/// legally replace a release root while holding the shared lock. Re-reading the
/// archive marker and live links from the lock-normalized physical root keeps a
/// stale pre-worker snapshot from authorizing deletion of the new generation.
pub(crate) fn revalidate_for_repo_install(
    transition: Option<&Transition>,
    roots: &Roots,
    locked_repo_root: &Path,
) -> Result<Option<Transition>> {
    let Some(transition) = transition else {
        return Ok(None);
    };
    let mut refreshed = transition.clone();
    if refreshed.old.method == method::GITHUB_RELEASE {
        let install_base = cleanup::install_root_for_repo(locked_repo_root, &refreshed.old.name)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cannot derive install root from acquired checkout-lock path",
                )
            })?;
        let explicit =
            github_release_install::explicit_archive_state(&install_base, &refreshed.old.name)?;
        let current_identity = file_identity(locked_repo_root)?;
        let checkout_metadata_present = path_entry_exists(&locked_repo_root.join(".git"))?;
        refreshed.archive_state = if explicit == ArchiveState::Proven {
            ArchiveState::Proven
        } else if !checkout_metadata_present
            && refreshed.archive_state == ArchiveState::Proven
            && same_file_identity(refreshed.archive_root_identity, current_identity)
        {
            github_release_install::archive_state(
                &roots.state_dir,
                &install_base,
                &roots.bin_dir.join(&refreshed.old.cmd),
                &refreshed.old.name,
            )?
        } else {
            // Legacy archive proof is path-based: a public symlink can keep
            // resolving after another writer replaces the directory at the
            // same name. It authorizes only the exact root generation whose
            // identity was observed before waiting for the checkout lock.
            ArchiveState::None
        };
        refreshed.archive_root_identity = current_identity;
    }
    Ok(Some(refreshed))
}

// Snapshot one real directory generation on Unix; platforms without a stable
// inode API intentionally cannot retain legacy path-only archive proof.
fn file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Ok(None);
    }

    #[cfg(unix)]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

// Require a present, unchanged Unix identity rather than allowing None == None
// to become accidental ownership evidence.
fn same_file_identity(before: Option<FileIdentity>, current: Option<FileIdentity>) -> bool {
    #[cfg(unix)]
    {
        before.is_some() && before == current
    }
    #[cfg(not(unix))]
    {
        let _ = (before, current);
        false
    }
}

// Returns whether the still-configured old identity no longer owns its saved command here.
fn configured_identity_relinquished_command(
    old: &ManifestEntry,
    entries: &[Entry],
    env: &RuntimeEnv,
    pkg_mgr: &str,
) -> bool {
    entries.iter().any(|entry| {
        entry.name == old.name
            && (entry.cmd != old.cmd
                || platform::filter_match(&entry.filter, env) != platform::FilterMatch::Match
                || (entry.method == method::PKG
                    && config::resolve_override_for_runtime(
                        &entry.name,
                        &entry.aliases,
                        Some(pkg_mgr),
                        env.is_android(),
                    ) == "NONE"))
    })
}

// Probe a no-follow ownership marker while preserving all errors except true
// absence; a malformed .git entry must still disqualify legacy archive proof.
fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Recovers any interrupted raw-release command publication visible to this run.
pub(crate) fn recover_pending_publications(
    entries: &[Entry],
    manifest: &Manifest,
    manifest_path: &Path,
    roots: &Roots,
) -> Result<()> {
    #[cfg(unix)]
    {
        let mut commands = BTreeSet::new();
        commands.extend(
            entries
                .iter()
                .filter(|entry| !entry.cmd.is_empty())
                .map(|entry| entry.cmd.as_str()),
        );
        commands.extend(
            manifest
                .effective_entries()
                .into_iter()
                .filter(|entry| !entry.cmd.is_empty())
                .map(|entry| entry.cmd.as_str()),
        );
        for command in commands {
            if config::valid_cmd_basename(command) {
                recover_public_transition(manifest_path, &roots.bin_dir.join(command), roots)?;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (entries, manifest, manifest_path, roots);
    }
    Ok(())
}

#[cfg(unix)]
fn public_transition_path(public: &Path) -> Result<PathBuf> {
    let parent = public.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "public command has no parent directory",
        )
    })?;
    let name = public
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "public command has no UTF-8 basename",
            )
        })?;
    Ok(parent.join(format!(".{name}.shdeps-public-transition-v1")))
}

#[cfg(unix)]
fn expected_public_source(entry: &ManifestEntry) -> PathBuf {
    if entry.method == method::GITHUB_REPO {
        PathBuf::from(&entry.install_path)
            .join("bin")
            .join(&entry.cmd)
    } else {
        PathBuf::from(&entry.install_path)
    }
}

#[cfg(unix)]
fn public_symlink_matches(path: &Path, source: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_symlink()
            && fs::read_link(path).is_ok_and(|target| target == source)
    })
}

#[cfg(unix)]
fn remove_recorded_transition_symlink(path: &Path, source: &Path) -> Result<bool> {
    if !public_symlink_matches(path, source) {
        return Ok(false);
    }
    // Transition swap names are unique and, once journaled, are already the
    // recovery location. Moving one through the generic unlink quarantine
    // would create an unrecorded second crash state.
    fs::remove_file(path)?;
    Ok(true)
}

#[cfg(unix)]
fn read_public_transition(public: &Path) -> Result<Option<(PathBuf, PublicTransitionRecord)>> {
    let journal = public_transition_path(public)?;
    let metadata = match fs::symlink_metadata(&journal) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "public command transition is not a private record: {}",
                journal.display()
            ),
        )
        .into());
    }
    let bytes = crate::state::read_private_bounded(&journal, MAX_PUBLIC_TRANSITION_RECORD_BYTES)?;
    let record: PublicTransitionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed public command transition record: {error}"),
        )
    })?;
    validate_public_transition(public, &journal, &record)?;
    Ok(Some((journal, record)))
}

#[cfg(unix)]
fn validate_public_transition(
    public: &Path,
    journal: &Path,
    record: &PublicTransitionRecord,
) -> Result<()> {
    if record.format != PUBLIC_TRANSITION_FORMAT
        || record.public != public
        || record.old.name != record.new.name
        || record.old.cmd != record.new.cmd
        || record.old.cmd
            != public
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
        || record.old.method != method::GITHUB_RELEASE
        || !method::is_symlink_install_root(&record.new.method)
        || expected_public_source(&record.new) != record.source
        || public_transition_path(public)? != journal
        || record.swap.parent() != public.parent()
        || record
            .swap
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| {
                !name.starts_with(&format!(".{}.shdeps-public-swap.", record.old.cmd))
            })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "public command transition does not belong to {}",
                public.display()
            ),
        )
        .into());
    }
    cleanup::validate_manifest_artifact_entry(&record.old)?;
    cleanup::validate_manifest_artifact_entry(&record.new)?;
    Ok(())
}

#[cfg(unix)]
fn remove_public_transition_journal(journal: &Path) -> Result<()> {
    match fs::remove_file(journal) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_transitioned_binlink(record: &PublicTransitionRecord, roots: &Roots) -> Result<()> {
    if record.new.method != method::GITHUB_REPO {
        return Ok(());
    }
    let state_path = link_state::path(&roots.state_dir, &record.new.name, Kind::Bin);
    let mut links = link_state::read(&state_path)?;
    if !links.contains(&record.public) {
        links.push(record.public.clone());
        links.sort();
        links.dedup();
        link_state::write(&state_path, &links)?;
    }
    Ok(())
}

#[cfg(unix)]
fn recover_public_transition(manifest_path: &Path, public: &Path, roots: &Roots) -> Result<()> {
    let Some((journal, record)) = read_public_transition(public)? else {
        return Ok(());
    };
    let swap = record.swap.clone();
    let installed = manifest::read(manifest_path)?;
    let current = installed.get(&record.old.name);
    let manifest_is_old = current == Some(&record.old);
    let manifest_is_new = current == Some(&record.new);
    if !manifest_is_old && !manifest_is_new {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "public command transition cannot classify manifest row for {}",
                record.old.name
            ),
        )
        .into());
    }

    let public_is_old = cleanup::regular_file_matches_after_rename(public, &record.expected)?;
    let public_is_new = public_symlink_matches(public, &record.source);
    let swap_is_old = cleanup::regular_file_matches_after_rename(&swap, &record.expected)?;
    let swap_is_new = public_symlink_matches(&swap, &record.source);
    let swap_exists = fs::symlink_metadata(&swap).is_ok();

    if manifest_is_old {
        if public_is_new && swap_is_old {
            crate::repo_transition::rename_exchange(public, &swap)?;
            if !cleanup::regular_file_matches_after_rename(public, &record.expected)? {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "restored public command does not match its transition record",
                )
                .into());
            }
        } else if !public_is_old {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "public command transition found an unrecognized live path at {}",
                    public.display()
                ),
            )
            .into());
        }

        if public_symlink_matches(&swap, &record.source) {
            remove_recorded_transition_symlink(&swap, &record.source)?;
        } else if fs::symlink_metadata(&swap).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "public command transition preserved an unexpected rollback object at {}",
                    swap.display()
                ),
            )
            .into());
        }
        remove_public_transition_journal(&journal)?;
        return Ok(());
    }

    if public_is_old && swap_is_new {
        crate::repo_transition::rename_exchange(public, &swap)?;
    } else if !public_is_new {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "committed public command transition found an unrecognized live path at {}",
                public.display()
            ),
        )
        .into());
    }
    if !public_symlink_matches(public, &record.source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "committed public command does not match its transition record",
        )
        .into());
    }
    ensure_transitioned_binlink(&record, roots)?;
    if cleanup::regular_file_matches_after_rename(&swap, &record.expected)? {
        // `swap` is already a unique, journal-recorded recovery path whose
        // exact file generation was validated above. Moving it through the
        // generic cleanup quarantine would create a second, unjournaled crash
        // window, so retire the recorded generation in place.
        fs::remove_file(&swap)?;
    } else if swap_exists && !swap_is_new {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "public command transition preserved an unexpected commit object at {}",
                swap.display()
            ),
        )
        .into());
    } else if swap_is_new {
        remove_recorded_transition_symlink(&swap, &record.source)?;
    }
    remove_public_transition_journal(&journal)?;
    Ok(())
}

#[cfg(unix)]
fn write_public_transition_record(journal: &Path, record: &PublicTransitionRecord) -> Result<()> {
    let parent = journal.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "public command transition has no parent directory",
        )
    })?;
    let name = journal
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "public command transition has no UTF-8 basename",
            )
        })?;
    let mut encoded = serde_json::to_string_pretty(record)?;
    encoded.push('\n');
    for _ in 0..16 {
        let nonce = PUBLIC_TRANSITION_NONCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.tmp.{}.{}", std::process::id(), nonce));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let write = file
            .write_all(encoded.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        let publish = crate::repo_transition::rename_noreplace(&temp, journal);
        let _ = fs::remove_file(&temp);
        return publish.map_err(Into::into);
    }
    Err(std::io::Error::other("could not allocate public transition record staging path").into())
}

#[cfg(unix)]
fn begin_public_transition(
    transition: &Transition,
    new: ManifestEntry,
    roots: &Roots,
    manifest_path: &Path,
) -> Result<PathBuf> {
    let public = roots.bin_dir.join(&new.cmd);
    recover_public_transition(manifest_path, &public, roots)?;
    let expected = transition
        .cleanup_evidence
        .public_regular_identity()
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw release transition has no public command identity",
            )
        })?;
    if cleanup::regular_file_identity(&public)?.as_ref() != Some(&expected) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "public command changed before transition publication",
        )
        .into());
    }
    let source = expected_public_source(&new);
    let journal = public_transition_path(&public)?;
    let mut swap = None;
    for _ in 0..16 {
        let nonce = PUBLIC_TRANSITION_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = public.with_file_name(format!(
            ".{}.shdeps-public-swap.{}.{}",
            new.cmd,
            std::process::id(),
            nonce
        ));
        match std::os::unix::fs::symlink(&source, &candidate) {
            Ok(()) => {
                swap = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let swap = swap
        .ok_or_else(|| std::io::Error::other("could not allocate public transition swap path"))?;
    let record = PublicTransitionRecord {
        format: PUBLIC_TRANSITION_FORMAT.to_owned(),
        public: public.clone(),
        swap: swap.clone(),
        source: source.clone(),
        old: transition.old.clone(),
        new,
        expected,
    };
    if let Err(error) = write_public_transition_record(&journal, &record) {
        let _ = remove_recorded_transition_symlink(&swap, &source);
        return Err(error);
    }
    if let Err(error) = crate::repo_transition::rename_exchange(&public, &swap) {
        return match recover_public_transition(manifest_path, &public, roots) {
            Ok(()) => Err(error.into()),
            Err(recovery) => Err(std::io::Error::other(format!(
                "public command exchange failed ({error}); recovery also failed ({recovery})"
            ))
            .into()),
        };
    }
    if !cleanup::regular_file_matches_after_rename(&swap, &record.expected)? {
        let recovery = recover_public_transition(manifest_path, &public, roots);
        return Err(std::io::Error::other(match recovery {
            Ok(()) => "public command changed during atomic publication; restored it".to_owned(),
            Err(error) => format!(
                "public command changed during atomic publication; recovery failed: {error}"
            ),
        })
        .into());
    }
    Ok(public)
}

/// Runs an installer with transition-only public-bin preparation.
pub(crate) fn install_with_prepared(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    manifest_path: &Path,
    install: impl FnOnce(&Path) -> Result<Item>,
) -> Result<Item> {
    install_with_prepared_and_commit(
        entry,
        transition,
        roots,
        manifest_path,
        install,
        manifest::upsert,
    )
}

fn install_with_prepared_and_commit(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    manifest_path: &Path,
    install: impl FnOnce(&Path) -> Result<Item>,
    commit_manifest: impl FnOnce(&Path, ManifestEntry) -> Result<()>,
) -> Result<Item> {
    let prepared_manifest = prepare_manifest(entry, transition, roots, manifest_path)?;
    let mut item = match install(prepared_manifest.as_deref().unwrap_or(manifest_path)) {
        Ok(item) => item,
        Err(error) => {
            return Err(with_prepared_cleanup(
                error,
                prepared_manifest.as_deref(),
                &roots.state_dir,
            ));
        }
    };
    if item.failed {
        if let Err(error) = remove_prepared_manifest(prepared_manifest.as_deref(), &roots.state_dir)
        {
            item.detail = format!(
                "{}; removing the prepared manifest also failed: {error}",
                item.detail
            );
        }
        return Ok(item);
    }

    let install_manifest = prepared_manifest.as_deref().unwrap_or(manifest_path);
    let committed_entry = match prepared_manifest.as_ref() {
        Some(_) => match installed_entry(install_manifest, entry) {
            Ok(entry) => Some(entry),
            Err(error) => {
                return Err(with_prepared_cleanup(
                    error,
                    prepared_manifest.as_deref(),
                    &roots.state_dir,
                ));
            }
        },
        None => None,
    };
    let publication =
        publish_replacement_public_bin(entry, transition, roots, manifest_path, install_manifest);
    let mut pending_public = None;
    match publication {
        Ok(PublicPublication::Warning(warning)) => {
            item.status = crate::update::ItemStatus::Warning;
            item.reason = crate::update::ItemReason::Other;
            item.detail = format!("{}; {warning}", item.detail);
        }
        Ok(PublicPublication::None) => {}
        #[cfg(unix)]
        Ok(PublicPublication::Pending(public)) => pending_public = Some(public),
        Err(error) => {
            if prepared_manifest.is_none() {
                restore_failed(transition, manifest_path)?;
            }
            return Err(with_prepared_cleanup(
                error,
                prepared_manifest.as_deref(),
                &roots.state_dir,
            ));
        }
    }

    if let Some(committed_entry) = committed_entry {
        if let Err(error) = commit_manifest(manifest_path, committed_entry) {
            #[cfg(unix)]
            if let Some(public) = pending_public.as_deref() {
                if let Err(recovery) = recover_public_transition(manifest_path, public, roots) {
                    return Err(with_prepared_cleanup(
                        std::io::Error::other(format!(
                            "manifest commit failed ({error}); public command recovery also failed ({recovery})"
                        ))
                        .into(),
                        prepared_manifest.as_deref(),
                        &roots.state_dir,
                    ));
                }
            }
            return Err(with_prepared_cleanup(
                error,
                prepared_manifest.as_deref(),
                &roots.state_dir,
            ));
        }
    }
    #[cfg(unix)]
    if let Some(public) = pending_public.as_deref() {
        if let Err(error) = recover_public_transition(manifest_path, public, roots) {
            item.status = crate::update::ItemStatus::Warning;
            item.reason = crate::update::ItemReason::Other;
            item.detail = format!(
                "{}; committed the new provider but public command finalization needs recovery: {error}",
                item.detail
            );
        }
    }
    if let Err(error) = remove_prepared_manifest(prepared_manifest.as_deref(), &roots.state_dir) {
        item.status = crate::update::ItemStatus::Warning;
        item.reason = crate::update::ItemReason::Other;
        item.detail = format!(
            "{}; removing the prepared manifest failed: {error}",
            item.detail
        );
    }
    Ok(item)
}

fn with_prepared_cleanup(
    primary: crate::Error,
    prepared_manifest: Option<&Path>,
    state_dir: &Path,
) -> crate::Error {
    match remove_prepared_manifest(prepared_manifest, state_dir) {
        Ok(()) => primary,
        Err(cleanup) => std::io::Error::other(format!(
            "{primary}; removing the prepared manifest also failed: {cleanup}"
        ))
        .into(),
    }
}

fn prepare_manifest(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    manifest_path: &Path,
) -> Result<Option<PathBuf>> {
    prepare_manifest_with_nonce(entry, transition, roots, manifest_path, || {
        MANIFEST_STAGE_NONCE.fetch_add(1, Ordering::Relaxed)
    })
}

fn prepare_manifest_with_nonce(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    manifest_path: &Path,
    mut next_nonce: impl FnMut() -> u64,
) -> Result<Option<PathBuf>> {
    let Some(transition) = transition else {
        return Ok(None);
    };
    if transition.old.method != method::GITHUB_RELEASE
        || transition.old.cmd != entry.cmd
        || !method::is_symlink_install_root(&entry.method)
    {
        return Ok(None);
    }
    if transition.archive_state == ArchiveState::Ambiguous {
        return Err(std::io::Error::other(format!(
            "refusing to replace ambiguous legacy release command: {}",
            roots.bin_dir.join(&entry.cmd).display()
        ))
        .into());
    }

    let parent = manifest_path.parent().unwrap_or(&roots.state_dir);
    let name = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("manifest");
    for _ in 0..16 {
        let nonce = next_nonce();
        let prepared = parent.join(format!(
            ".{name}.shdeps-transition.{}.{}",
            std::process::id(),
            nonce
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&prepared) {
            Ok(mut file) => {
                let content = format!("{}\n", transition.old.line());
                let write = file
                    .write_all(content.as_bytes())
                    .and_then(|()| file.sync_all());
                drop(file);
                if let Err(error) = write {
                    let _ = fs::remove_file(&prepared);
                    return Err(error.into());
                }
                return Ok(Some(prepared));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::other("could not allocate prepared manifest path").into())
}

fn installed_entry(manifest_path: &Path, entry: &Entry) -> Result<ManifestEntry> {
    let installed = manifest::read(manifest_path)?;
    let current = installed.get(&entry.name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "successful transition did not record a manifest row",
        )
    })?;
    if current.method != entry.method || current.cmd != entry.cmd {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "successful transition recorded an unexpected manifest row",
        )
        .into());
    }
    Ok(current.clone())
}

fn remove_prepared_manifest(path: Option<&Path>, state_dir: &Path) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == state_dir {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        parent = dir.parent();
    }
    Ok(())
}

/// Cleans old artifacts after a new method has successfully recorded itself.
pub(crate) fn cleanup_successful(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<Option<String>> {
    let Some(transition) = transition else {
        return Ok(None);
    };
    if transition.old.method == method::GITHUB_REPO && locked_repo_root.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo transition cleanup requires an acquired checkout-lock root",
        )
        .into());
    }

    Ok(cleanup_snapshot(entry, transition, roots, locked_repo_root)
        .err()
        .map(|error| error.to_string()))
}

/// Restores the old manifest row after a transition install fails.
pub(crate) fn restore_failed(transition: Option<&Transition>, manifest_path: &Path) -> Result<()> {
    if let Some(transition) = transition {
        manifest::upsert(manifest_path, transition.old.clone())?;
    }
    Ok(())
}

fn publish_replacement_public_bin(
    entry: &Entry,
    transition: Option<&Transition>,
    roots: &Roots,
    real_manifest_path: &Path,
    install_manifest_path: &Path,
) -> Result<PublicPublication> {
    let Some(transition) = transition else {
        return Ok(PublicPublication::None);
    };

    if transition.old.method != method::GITHUB_RELEASE
        || transition.old.cmd != entry.cmd
        || !method::is_symlink_install_root(&entry.method)
    {
        return Ok(PublicPublication::None);
    }

    let original = roots.bin_dir.join(&entry.cmd);
    match transition.archive_state {
        ArchiveState::Proven if github_release_install::is_non_symlink(&original) => {
            // Archive installs never own a regular launcher they preserved.
            // Moving it aside here would let the new method replace it and the
            // success path would then discard the only copy.
            return Ok(PublicPublication::None);
        }
        ArchiveState::Ambiguous => {
            return Err(std::io::Error::other(format!(
                "refusing to replace ambiguous legacy release command: {}",
                original.display()
            ))
            .into());
        }
        ArchiveState::None | ArchiveState::Proven => {}
    }
    let installed = manifest::read(install_manifest_path)?;
    let current = installed.get(&entry.name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "successful transition did not record a manifest row",
        )
    })?;
    if current.method != entry.method || current.cmd != entry.cmd {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "successful transition recorded an unexpected manifest row",
        )
        .into());
    }
    let source = if entry.method == method::GITHUB_REPO {
        PathBuf::from(&current.install_path)
            .join("bin")
            .join(&entry.cmd)
    } else {
        PathBuf::from(&current.install_path)
    };
    if !crate::process::executable_path(&source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "successful transition produced no executable command at {}",
                source.display()
            ),
        )
        .into());
    }

    let Some(expected) = transition.cleanup_evidence.public_regular_identity() else {
        if public_symlink_matches(&original, &source) {
            return Ok(PublicPublication::None);
        }
        return Ok(PublicPublication::Warning(format!(
            "public command is no longer the snapshotted raw release; preserved it at {}",
            original.display()
        )));
    };
    if cleanup::regular_file_identity(&original)?.as_ref() != Some(expected) {
        return Ok(PublicPublication::Warning(format!(
            "public command changed during method transition; preserved replacement at {}",
            original.display()
        )));
    }

    #[cfg(not(unix))]
    {
        let _ = source;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic raw-command transition is unsupported on this platform",
        )
        .into());
    }

    #[cfg(unix)]
    {
        let public =
            begin_public_transition(transition, current.clone(), roots, real_manifest_path)?;
        Ok(PublicPublication::Pending(public))
    }
}

fn cleanup_snapshot(
    entry: &Entry,
    transition: &Transition,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<()> {
    let preserve = preserve_paths(entry, roots, locked_repo_root)?;

    match transition.old.method.as_str() {
        method::PKG => {
            // System packages are not shdeps-owned. Once another method has
            // succeeded, the manifest swap is enough; the OS package remains
            // available for anything else on the machine that might use it.
            crate::package_proof::remove(&roots.state_dir, &transition.old.name)?;
        }
        method::GITHUB_REPO => {
            let install_path = locked_repo_root.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "github:repo transition cleanup requires an acquired checkout-lock root",
                )
            })?;
            let preserves_replacement_root = preserve.contains(install_path);
            if !preserves_replacement_root
                && !transition
                    .cleanup_evidence
                    .authorizes_managed_root(&roots.install_dir, install_path)
            {
                return Err(std::io::Error::other(format!(
                    "repository root changed after transition evidence was captured: {}",
                    install_path.display()
                ))
                .into());
            }
            let logical_root = roots.install_dir.join(config::canonical_name(
                &transition.old.name,
                method::GITHUB_REPO,
            ));
            let owner_roots = if preserves_replacement_root {
                let mut roots_for_links = vec![install_path.to_path_buf()];
                if transition
                    .cleanup_evidence
                    .logical_base_matches(&roots.install_dir)
                {
                    roots_for_links.push(logical_root);
                }
                roots_for_links
            } else {
                transition
                    .cleanup_evidence
                    .owner_roots(&roots.install_dir, logical_root)
            };
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Bin,
                &transition.bin_links,
                &preserve,
                &owner_roots,
            )?;
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Extras,
                &transition.extra_links,
                &preserve,
                &owner_roots,
            )?;

            // Repository ownership comes from the validated dependency name,
            // never the human-editable recorded install path.
            if !preserve.contains(install_path)
                && transition.cleanup_evidence.remove_managed_root()?
            {
                if let Some(install_root) = transition.cleanup_evidence.physical_install_base() {
                    remove_empty_install_parents(install_path, install_root)?;
                }
            }

            let legacy_bin = roots.bin_dir.join(config::short_name(&transition.old.name));
            if !preserve.contains(&legacy_bin) {
                let cleanup_roots = crate::cleanup::Roots {
                    state_dir: roots.state_dir.clone(),
                    install_dir: roots.install_dir.clone(),
                    bin_dir: roots.bin_dir.clone(),
                };
                crate::cleanup::remove_legacy_repo_command(
                    &transition.old,
                    &cleanup_roots,
                    &owner_roots,
                )?;
            }
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        binary if method::is_binary_install_root(binary) => {
            let public_bin = roots.bin_dir.join(&transition.old.cmd);
            let logical_root = roots.install_dir.join(&transition.old.name);
            let owner_roots = transition
                .cleanup_evidence
                .owner_roots(&roots.install_dir, logical_root);
            let preserve_public_launcher = binary == method::GITHUB_RELEASE
                && transition.archive_state != ArchiveState::None
                && github_release_install::is_non_symlink(&public_bin);
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Bin,
                &transition.bin_links,
                &preserve,
                &owner_roots,
            )?;
            unlink_snapshot(
                &roots.state_dir,
                &transition.old.name,
                Kind::Extras,
                &transition.extra_links,
                &preserve,
                &owner_roots,
            )?;
            if !preserve_public_launcher && !preserve.contains(&public_bin) {
                let removed = cleanup::unlink_owned_symlink(&public_bin, &owner_roots)?;
                if !removed && binary == method::GITHUB_RELEASE {
                    if let Some(identity) = transition.cleanup_evidence.public_regular_identity() {
                        cleanup::remove_owned_regular_file(&public_bin, identity)?;
                    }
                }
            }

            let owns_install_root = binary != method::GITHUB_RELEASE
                || transition.archive_state == ArchiveState::Proven;
            if owns_install_root
                && !owner_roots.iter().any(|root| preserve.contains(root))
                && transition.cleanup_evidence.remove_managed_root()?
            {
                if let (Some(install_root), Some(install_base)) = (
                    transition.cleanup_evidence.managed_install_root(),
                    transition.cleanup_evidence.physical_install_base(),
                ) {
                    remove_empty_install_parents(install_root, install_base)?;
                }
            }
            remove_stamps(&roots.state_dir, &transition.old.name)?;
        }
        method::CUSTOM => remove_stamps(&roots.state_dir, &transition.old.name)?,
        _ => {}
    }

    Ok(())
}

fn preserve_paths(
    entry: &Entry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<BTreeSet<PathBuf>> {
    let mut preserve = BTreeSet::new();

    // New-method link state may live at the same `<name>.links` path as the
    // old method. Read it after the install succeeds so transition cleanup can
    // remove only pre-existing links without deleting the freshly linked public
    // command, man page, or completion.
    if matches!(
        entry.method.as_str(),
        method::GITHUB_REPO | method::GITHUB_RELEASE
    ) {
        for kind in [Kind::Bin, Kind::Extras] {
            if let Ok(links) =
                link_state::read(&link_state::path(&roots.state_dir, &entry.name, kind))
            {
                preserve.extend(links);
            }
        }
    }

    match entry.method.as_str() {
        symlink if method::is_symlink_install_root(symlink) => {
            preserve.insert(roots.bin_dir.join(&entry.cmd));
            preserve.insert(roots.install_dir.join(&entry.name));
            preserve.insert(locked_repo_root.map_or_else(
                || crate::cleanup::physical_install_root(&roots.install_dir).join(&entry.name),
                Path::to_path_buf,
            ));
        }
        method::GITHUB_RELEASE => {
            let public_bin = roots.bin_dir.join(&entry.cmd);
            preserve.insert(public_bin.clone());

            // A repo-to-release transition holds the checkout lock for one
            // physical root. Treat that path as the ownership authority all
            // the way through cleanup: the configured install directory may be
            // a symlink that is retargeted after the release installer writes
            // the new archive but before this preservation check runs.
            let install_base = match locked_repo_root {
                Some(repo_root) => crate::cleanup::install_root_for_repo(repo_root, &entry.name)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cannot derive install root from acquired checkout-lock path",
                        )
                    })?,
                None => crate::cleanup::physical_install_root(&roots.install_dir),
            };
            let install_root = locked_repo_root
                .map(Path::to_path_buf)
                .unwrap_or_else(|| install_base.join(&entry.name));
            if github_release_install::archive_state(
                &roots.state_dir,
                &install_base,
                &public_bin,
                &entry.name,
            )? == ArchiveState::Proven
                || release_install_root_is_owned(&install_root, &public_bin)
            {
                preserve.insert(roots.install_dir.join(&entry.name));
                preserve.insert(install_root);
            }
        }
        _ => {}
    }

    Ok(preserve)
}

fn release_install_root_is_owned(path: &Path, public_bin: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };

    // `github:release` has two ownership modes. Raw/compressed single-file
    // assets own only the public binary, while archive assets replace the
    // managed install root with a real directory and then symlink the public
    // command into it. During a method transition from `github:repo`, that same
    // path may still be a symlink to a local development checkout. Preserving
    // the symlink would leave stale repo state behind and make `dep-root` point
    // at a clone even though the configured method is now release-based. A real
    // directory is only safe to keep when the public command actually resolves
    // into it. A repo -> raw-release transition can leave an old real checkout
    // directory at the same path; preserving it would make the manifest say
    // release while `dep-root` and cleanup still see stale repo-owned files.
    // The public command is the legacy proof of archive ownership because raw
    // release installs write a regular bin file, while old archive installs
    // exposed the selected binary from inside the extracted root. New archives
    // carry an explicit marker handled by `archive_state` above.
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }

    points_into(public_bin, path)
}

fn points_into(path: &Path, root: &Path) -> bool {
    // Fully resolve all symlink levels so chained symlinks (e.g. bin/tool ->
    // share/name/current -> share/name/v1.2.3/bin/tool) are handled correctly.
    // canonicalize fails for broken symlinks or targets that don't exist yet
    // during concurrent installs, so fall back to single-level read_link in
    // that case — the one-level result is still useful for the primary check.
    let resolved = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            if !fs::symlink_metadata(path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                path.to_path_buf()
            } else {
                match fs::read_link(path) {
                    Ok(target) if target.is_absolute() => target,
                    Ok(target) => path.parent().unwrap_or_else(|| Path::new("/")).join(target),
                    Err(_) => return false,
                }
            }
        }
    };

    // Canonicalize root as well — intermediate path components (e.g. a home
    // directory exposed through a /home -> /usr/home symlink) would otherwise
    // make the starts_with prefix check fail even for a correct containment.
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolved.starts_with(canonical_root)
}

fn unlink_snapshot(
    state_dir: &Path,
    name: &str,
    kind: Kind,
    snapshot: &[PathBuf],
    preserve: &BTreeSet<PathBuf>,
    owner_roots: &[PathBuf],
) -> Result<()> {
    for link in snapshot {
        if preserve.contains(link) {
            continue;
        }
        cleanup::unlink_owned_symlink(link, owner_roots)?;
    }

    // Clear the snapshot entries from the link-state file rather than
    // requiring byte-exact equality. The pre-fix code only cleared when
    // the on-disk state matched the snapshot exactly; if any entry in
    // the snapshot had been deleted externally (manual edit, partial
    // failure of a prior run, parallel install), the comparison failed
    // and the state file kept stale entries pointing at paths that no
    // longer exist. Recompute the remainder by filtering the snapshot
    // out of the current state and writing only what is left.
    let state_path = link_state::path(state_dir, name, kind);
    let current = link_state::read(&state_path)?;
    let snapshot_set: BTreeSet<&PathBuf> = snapshot
        .iter()
        .filter(|link| !preserve.contains(*link))
        .collect();
    let remainder: Vec<PathBuf> = current
        .into_iter()
        .filter(|link| !snapshot_set.contains(link))
        .collect();
    link_state::write(&state_path, &remainder)?;
    Ok(())
}

fn remove_stamps(state_dir: &Path, name: &str) -> Result<()> {
    cleanup::remove_stamps(state_dir, name, &mut cleanup::Summary::default())
}

fn remove_empty_install_parents(path: &Path, install_dir: &Path) -> Result<()> {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == install_dir || dir == Path::new("/") {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => break,
        }
        parent = dir.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::{
        Transition, begin_public_transition, by_name, cleanup_snapshot, install_with_prepared,
        install_with_prepared_and_commit, points_into, prepare_manifest_with_nonce,
        public_transition_path, recover_public_transition, reject_identity_handoffs,
        unlink_snapshot,
    };
    use crate::config::{Entry, parse_entry};
    use crate::github_release_install::{self, ArchiveState};
    use crate::link_state::{self, Kind, ReconcileLink};
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::platform::RuntimeEnv;
    use crate::runtime::Roots;
    use crate::update::{Item, ItemReason};
    use std::collections::BTreeSet;

    fn cleanup_snapshot_for_test(
        entry: &Entry,
        transition: &Transition,
        roots: &Roots,
    ) -> crate::Result<()> {
        let cleanup_roots = crate::cleanup::Roots {
            state_dir: roots.state_dir.clone(),
            install_dir: roots.install_dir.clone(),
            bin_dir: roots.bin_dir.clone(),
        };
        let mut transition = transition.clone();
        transition.cleanup_evidence =
            crate::cleanup::capture_evidence(&transition.old, &cleanup_roots)?;
        let repo_root = (transition.old.method == crate::method::GITHUB_REPO)
            .then(|| crate::cleanup::safe_repo_root(&transition.old, &cleanup_roots))
            .flatten();
        cleanup_snapshot(entry, &transition, roots, repo_root.as_deref())
    }

    #[test]
    fn identity_handoff_guard_ignores_inactive_replacement() {
        let manifest = Manifest::parse("owner/old|github:release|tool|/tmp/old\n");
        let entries = [parse_entry("replacement|pkg|tool|-|os:macos", Some("apt"))];

        reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    fn identity_handoff_guard_allows_same_name_method_transition() {
        let manifest = Manifest::parse("tool|github:release|tool|/tmp/tool\n");
        let entries = [parse_entry("tool|pkg|tool|-|-", Some("apt"))];

        reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    fn identity_handoff_guard_rejects_inactive_old_name_with_active_replacement() {
        let manifest = Manifest::parse("owner/old|github:release|tool|/tmp/old\n");
        let entries = [
            parse_entry("owner/old|github:release|tool|-|os:macos", Some("apt")),
            parse_entry("replacement|pkg|tool|-|-", Some("apt")),
        ];

        let error = reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap_err();

        assert!(error.to_string().contains("owner/old"));
        assert!(error.to_string().contains("replacement"));
        assert!(error.to_string().contains("remove the old declaration"));
        assert!(error.to_string().contains("shdeps prune"));
    }

    #[test]
    fn identity_handoff_guard_allows_recorded_platform_replacement() {
        let manifest = Manifest::parse(
            "platform-package|pkg|tool|\n\
             owner/tool|github:release|tool|/tmp/tool\n",
        );
        let entries = [
            parse_entry("platform-package|pkg|tool|-|os:macos", Some("apt")),
            parse_entry("owner/tool|github|tool|-|os:!macos", Some("apt")),
        ];

        reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    fn identity_handoff_guard_allows_recorded_command_reassignment() {
        let manifest = Manifest::parse(
            "python|pkg|python3|\n\
             python-minimum|custom|python3|\n",
        );
        let entries = [
            parse_entry("python|pkg|python|-|-", Some("apt")),
            parse_entry("python-minimum|custom|python3|-|-", Some("apt")),
        ];

        reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    fn identity_handoff_guard_rejects_mismatched_recorded_replacement() {
        let manifest = Manifest::parse(
            "owner/old|github:release|tool|/tmp/old\n\
             replacement|custom|other|\n",
        );
        let entries = [
            parse_entry("owner/old|github:release|tool|-|os:macos", Some("apt")),
            parse_entry("replacement|custom|tool|-|-", Some("apt")),
        ];

        let error = reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap_err();

        assert!(error.to_string().contains("owner/old"));
        assert!(error.to_string().contains("replacement"));
    }

    #[test]
    fn identity_handoff_guard_rejects_recorded_rename_after_old_declaration_is_removed() {
        let manifest = Manifest::parse(
            "owner/old|github:release|tool|/tmp/old\n\
             replacement|custom|tool|\n",
        );
        let entries = [parse_entry("replacement|custom|tool|-|-", Some("apt"))];

        let error = reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap_err();

        assert!(error.to_string().contains("owner/old"));
        assert!(error.to_string().contains("replacement"));
        assert!(error.to_string().contains("shdeps prune"));
    }

    #[test]
    fn identity_handoff_guard_rejects_reuse_after_same_name_command_change() {
        let manifest = Manifest::parse("owner/old|github:release|tool|/tmp/old\n");
        let entries = [
            parse_entry("owner/old|github:release|other|-|-", Some("apt")),
            parse_entry("replacement|pkg|tool|-|-", Some("apt")),
        ];

        let error = reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap_err();

        assert!(error.to_string().contains("owner/old"));
        assert!(error.to_string().contains("replacement"));
    }

    #[test]
    fn identity_handoff_guard_rejects_two_new_active_command_claims() {
        let entries = [
            parse_entry("first|pkg|tool|-|-", Some("apt")),
            parse_entry("second|custom|tool|-|-", Some("apt")),
        ];

        let error = reject_identity_handoffs(
            &Manifest::default(),
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate active command claim"));
        assert!(error.to_string().contains("first"));
        assert!(error.to_string().contains("second"));
    }

    #[test]
    fn identity_handoff_guard_ignores_package_manager_override_skip() {
        let entries = [
            parse_entry("disabled|pkg|tool|apt:NONE|-", Some("apt")),
            parse_entry("provider|custom|tool|-|-", Some("apt")),
        ];

        reject_identity_handoffs(
            &Manifest::default(),
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    fn identity_handoff_guard_ignores_superseded_duplicate_manifest_row() {
        let manifest = Manifest::parse(
            "owner/old|github:release|tool|/tmp/tool\n\
             owner/old|github:release|other|/tmp/other\n",
        );
        let entries = [
            parse_entry("owner/old|github:release|other|-|-", Some("apt")),
            parse_entry("replacement|pkg|tool|-|-", Some("apt")),
        ];

        reject_identity_handoffs(
            &manifest,
            &entries,
            &RuntimeEnv::new("linux", "host"),
            "apt",
        )
        .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn points_into_follows_direct_symlink() {
        let dir = temp_dir("direct");
        let target = dir.join("root/bin/tool");
        let link = dir.join("bin/tool");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"binary").unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();

        assert!(points_into(&link, &dir.join("root")));
        assert!(!points_into(&link, &dir.join("other")));
    }

    #[test]
    #[cfg(unix)]
    fn points_into_follows_chained_symlinks() {
        // Two-hop chain: bin/tool -> middle/tool -> root/bin/tool. The old
        // single-level read_link would resolve to `middle/tool` and then check
        // starts_with("root"), which is false — incorrectly treating the archive
        // install root as unowned and deleting it during a method transition.
        let dir = temp_dir("chained");
        let final_target = dir.join("root/bin/tool");
        let intermediate = dir.join("middle/tool");
        let link = dir.join("bin/tool");
        fs::create_dir_all(final_target.parent().unwrap()).unwrap();
        fs::write(&final_target, b"binary").unwrap();
        fs::create_dir_all(intermediate.parent().unwrap()).unwrap();
        symlink(&final_target, &intermediate).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&intermediate, &link).unwrap();

        assert!(points_into(&link, &dir.join("root")));
        assert!(!points_into(&link, &dir.join("middle")));
    }

    #[test]
    #[cfg(unix)]
    fn points_into_returns_false_for_broken_symlink() {
        let dir = temp_dir("broken");
        let link = dir.join("bin/tool");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(dir.join("nonexistent"), &link).unwrap();

        assert!(!points_into(&link, &dir.join("root")));
    }

    #[test]
    #[cfg(unix)]
    fn unlink_snapshot_clears_only_snapshot_entries_keeping_others_intact() {
        // The pre-fix code only cleared the state file when its
        // contents matched the snapshot byte-for-byte. If anything
        // had diverged externally — entries appended by a later
        // install, entries trimmed by manual cleanup, etc. — the
        // equality failed and the file kept stale snapshot entries
        // pointing at links that were just deleted. Filtering out
        // snapshot entries from the current state always converges
        // toward the correct remainder.
        let dir = temp_dir("clear-semantics");
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = link_state::path(&state_dir, "owner/tool", Kind::Extras);

        let snapshot = vec![dir.join("a"), dir.join("b")];
        // External addition: state file contains snapshot + an extra
        // link that does NOT belong to this transition.
        let extra = dir.join("external-other-dep-link");
        let mut on_disk = snapshot.clone();
        on_disk.push(extra.clone());
        link_state::write(&state_path, &on_disk).unwrap();

        // Pre-create the snapshot symlinks so the unlinker has
        // something to remove (its inner is_symlink check is also
        // exercised here).
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        for link in &snapshot {
            symlink(&target, link).unwrap();
        }
        symlink(&target, &extra).unwrap();

        unlink_snapshot(
            &state_dir,
            "owner/tool",
            Kind::Extras,
            &snapshot,
            &BTreeSet::new(),
            std::slice::from_ref(&target),
        )
        .unwrap();

        let remaining = link_state::read(&state_path).unwrap();
        assert_eq!(
            remaining,
            vec![extra.clone()],
            "snapshot entries removed; foreign entries preserved"
        );
        assert!(extra.is_symlink(), "foreign link must still exist on disk");
    }

    #[test]
    #[cfg(unix)]
    fn transition_snapshot_recovers_prepublished_repo_command() {
        let dir = temp_dir("prepublished-repo-command");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let source = roots.install_dir.join("owner/tool/bin/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(public.parent().unwrap()).unwrap();
        fs::write(&source, "#!/bin/sh\n").unwrap();
        let bin_state = link_state::path(&roots.state_dir, "owner/tool", Kind::Bin);
        link_state::begin_reconcile(
            &bin_state,
            &[ReconcileLink::new(public.clone(), source.clone())],
        )
        .unwrap();
        symlink(&source, &public).unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:repo|tool|{}\n",
            roots.install_dir.join("owner/tool").display()
        ));
        let entry = parse_entry("owner/tool|cargo|tool|-|-", None);

        let transitions = by_name(&manifest, std::slice::from_ref(&entry), &roots).unwrap();
        let transition = transitions.get("owner/tool").unwrap();

        assert_eq!(transition.bin_links, [public]);
        assert!(
            !bin_state
                .with_file_name("tool.binlinks.reconcile-v1")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn unlink_snapshot_clears_state_even_when_external_entry_was_deleted() {
        // The complementary case: state file is missing one of the
        // snapshot entries (a prior partial cleanup removed it from
        // the file by hand). The byte-equality gate of the pre-fix
        // code would refuse to clear the file in this case, leaving
        // a stale entry pointing at a link the unlinker just removed.
        let dir = temp_dir("clear-with-missing");
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let state_path = link_state::path(&state_dir, "owner/tool", Kind::Extras);

        let snapshot = vec![dir.join("a"), dir.join("b")];
        // State file only has one of the two snapshot links.
        link_state::write(&state_path, std::slice::from_ref(&snapshot[0])).unwrap();
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        symlink(&target, &snapshot[0]).unwrap();

        unlink_snapshot(
            &state_dir,
            "owner/tool",
            Kind::Extras,
            &snapshot,
            &BTreeSet::new(),
            std::slice::from_ref(&target),
        )
        .unwrap();

        // After clearing snapshot[0] from a state that only contained
        // snapshot[0], the file should be empty and (per
        // link_state::write semantics) removed entirely.
        assert!(
            !state_path.exists(),
            "state file should be removed once no entries remain"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_ignores_tampered_repo_install_path() {
        // The `github:repo` cleanup path once trusted the recorded manifest
        // path. A transition with a tampered
        // `old.install_path = /<bystander>` would therefore remove that
        // bystander. Cleanup authority now comes from the validated dependency
        // name and the checkout-lock root, so the recorded path is inert.
        //
        // Beyond the bystander assertion, this test also verifies that the
        // rest of the `github:repo` cleanup branch still runs. Ignoring the
        // recorded path must not skip legacy public-bin reconciliation or
        // stamp cleanup.
        let dir = temp_dir("tampered-install-path");
        let bystander = dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("data"), "user-owned").unwrap();

        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        // Per `cleanup_snapshot` for `github:repo`, the legacy public
        // bin lives at `<bin_dir>/<short_name>`. `short_name` of
        // `owner/tool` is `tool`, so seed that file and expect it to
        // be removed once cleanup runs to completion.
        let legacy_bin = roots.bin_dir.join("tool");
        fs::write(&legacy_bin, "old-bin").unwrap();

        // Stamps live next to the owner directory: `remove_stamps`
        // resolves `state_dir/<name>.parent()` → `state_dir/owner`
        // and removes `<base_name>.<kind>.stamp` files there.
        let stamp_dir = roots.state_dir.join("owner");
        fs::create_dir_all(&stamp_dir).unwrap();
        let stamp_file = stamp_dir.join("tool.repo.stamp");
        fs::write(&stamp_file, "stamp").unwrap();

        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                bystander.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(bystander.exists(), "bystander dir must be preserved");
        assert!(
            bystander.join("data").exists(),
            "bystander contents must be preserved"
        );
        // `legacy_bin` is `<bin_dir>/tool`, which equals
        // `preserve_paths` for the new `github:release` entry
        // (`bin_dir/<entry.cmd>` = `bin_dir/tool`). The preserve set
        // protects this file — that's the correct behavior because
        // the new method's freshly installed bin lives at that path.
        // So the file should remain, untouched, after cleanup.
        assert!(
            legacy_bin.exists(),
            "preserved public bin must not be removed during transition"
        );
        // The stamp belongs to the old method and is not preserved;
        // it must be removed by `remove_stamps`. If a regression skipped the
        // rest of cleanup while ignoring the tampered path, this assertion
        // would catch it.
        assert!(
            !stamp_file.exists(),
            "old-method stamp must be removed even when install_path is tampered"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_removes_binary_method_tracked_binlinks() {
        let dir = temp_dir("binary-binlinks");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        let install_root = roots.install_dir.join("owner/tool");
        let target = install_root.join("bin/tool");
        let helper_target = install_root.join("bin/tool-helper");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "old").unwrap();
        fs::write(&helper_target, "old helper").unwrap();
        let public = roots.bin_dir.join("tool");
        let helper = roots.bin_dir.join("tool-helper");
        symlink(&target, &public).unwrap();
        symlink(&helper_target, &helper).unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            &[public.clone(), helper.clone()],
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.to_string_lossy(),
            ),
            bin_links: vec![public.clone(), helper.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(!public.exists());
        assert!(!helper.exists());
        assert!(!link_state::path(&roots.state_dir, "owner/tool", Kind::Bin).exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_regular_path_from_symlink_only_method() {
        let dir = temp_dir("binary-regular-replacement");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("sentinel"), "preserve").unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(&public, "replacement command").unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "cargo",
                "tool",
                install_root.display().to_string(),
            ),
            bin_links: vec![public.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "other".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_to_string(public).unwrap(), "replacement command");
        assert!(!install_root.exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_raw_command_replaced_after_transition_snapshot() {
        let dir = temp_dir("release-regular-replacement-after-snapshot");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("sentinel"), "preserve").unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(&public, "old raw release").unwrap();
        let manifest = Manifest::parse(&format!(
            "owner/tool|github:release|tool|{}\n",
            public.display()
        ));
        let new_entry = parse_entry("owner/tool|pkg|other|-|-", Some("apt"));
        let transitions = by_name(&manifest, std::slice::from_ref(&new_entry), &roots).unwrap();
        let replacement = roots.bin_dir.join(".tool.replacement");
        fs::write(&replacement, "foreign replacement").unwrap();
        fs::rename(&replacement, &public).unwrap();

        cleanup_snapshot_for_test(&new_entry, &transitions["owner/tool"], &roots).unwrap();

        assert_eq!(fs::read_to_string(public).unwrap(), "foreign replacement");
        assert_eq!(
            fs::read_to_string(install_root.join("sentinel")).unwrap(),
            "preserve"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_retargeted_old_command_symlink() {
        let dir = temp_dir("binary-retargeted-replacement");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let replacement = roots.install_dir.join("replacement/bin/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(replacement.parent().unwrap()).unwrap();
        fs::write(&replacement, "replacement command").unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        symlink(&replacement, &public).unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "cargo",
                "tool",
                install_root.display().to_string(),
            ),
            bin_links: vec![public.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "other".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_link(public).unwrap(), replacement);
        assert!(!install_root.exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_new_root_through_symlinked_install_dir() {
        let dir = temp_dir("binary-symlinked-install-dir");
        let physical = dir.join("physical-install");
        let logical = dir.join("logical-install");
        fs::create_dir_all(&physical).unwrap();
        symlink(&physical, &logical).unwrap();
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: logical.clone(),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let new_root = physical.join("owner/tool");
        let new_target = new_root.join("bin/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(new_target.parent().unwrap()).unwrap();
        fs::write(&new_target, "new command").unwrap();
        fs::write(new_root.join("sentinel"), "preserve").unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        symlink(&new_target, &public).unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.display().to_string(),
            ),
            bin_links: vec![public.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "cargo".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(
            fs::read_to_string(new_root.join("sentinel")).unwrap(),
            "preserve"
        );
        assert_eq!(fs::read_link(public).unwrap(), new_target);
        assert_eq!(fs::read_link(logical).unwrap(), physical);
    }

    #[test]
    fn cleanup_snapshot_retires_old_package_proof() {
        let dir = temp_dir("retire-package-proof");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        crate::package_proof::write(&roots.state_dir, "tool", "apt", "tool", "tool").unwrap();
        let transition = Transition {
            old: ManifestEntry::new("tool", "pkg", "tool", ""),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "tool".to_owned(),
            method: "custom".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(!crate::package_proof::path(&roots.state_dir, "tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_preserves_archive_regular_launcher() {
        let dir = temp_dir("preserve-archive-launcher");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let public = roots.bin_dir.join("tool");
        let install_root = roots.install_dir.join("owner/tool");
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        fs::write(&public, "user launcher").unwrap();
        fs::write(install_root.join("tool"), "archive binary").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&roots.install_dir, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:release",
                "tool",
                public.to_string_lossy(),
            ),
            bin_links: vec![public.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::Proven,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_to_string(&public).unwrap(), "user launcher");
        assert!(!install_root.exists());
        assert!(!link_state::path(&roots.state_dir, "owner/tool", Kind::Bin).exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_snapshot_keeps_preserved_binlinks_in_state() {
        let dir = temp_dir("preserve-binlink-state");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        let target = dir.join("target");
        let old_helper = roots.install_dir.join("owner/tool/bin/tool-helper");
        fs::create_dir_all(old_helper.parent().unwrap()).unwrap();
        fs::write(&target, "new").unwrap();
        fs::write(&old_helper, "old helper").unwrap();
        let public = roots.bin_dir.join("tool");
        let helper = roots.bin_dir.join("tool-helper");
        symlink(&target, &public).unwrap();
        symlink(&old_helper, &helper).unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                roots.install_dir.join("owner/tool").to_string_lossy(),
            ),
            bin_links: vec![public.clone(), helper.clone()],
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert!(public.exists());
        assert!(!helper.exists());
        assert_eq!(
            link_state::read(&link_state::path(&roots.state_dir, "owner/tool", Kind::Bin)).unwrap(),
            vec![public]
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_transition_preserves_unowned_regular_command() {
        use std::os::unix::fs::MetadataExt;

        let dir = temp_dir("repo-unowned-regular-command");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(install_root.join("bin")).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(install_root.join("bin/tool"), "managed command").unwrap();
        fs::write(&public, "generated client adapter").unwrap();
        let before = fs::metadata(&public).unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "generated client adapter"
        );
        let after = fs::metadata(&public).unwrap();
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        assert!(!install_root.exists());
    }

    #[test]
    fn repo_transition_cleanup_rejects_missing_lock_authority() {
        let dir = temp_dir("repo-transition-missing-lock");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("artifact"), "preserve\n").unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.display().to_string(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "pkg".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        let error = cleanup_snapshot(&new_entry, &transition, &roots, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires an acquired checkout-lock root")
        );
        assert!(install_root.join("artifact").exists());
    }

    #[test]
    #[cfg(unix)]
    fn repo_transition_preserves_new_root_through_symlinked_install_dir() {
        let dir = temp_dir("repo-transition-symlinked-install-root");
        let physical_install = dir.join("physical-install");
        let logical_install = dir.join("install-link");
        fs::create_dir_all(&physical_install).unwrap();
        symlink(&physical_install, &logical_install).unwrap();
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: logical_install.clone(),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let physical_root = physical_install.join("owner/tool");
        fs::create_dir_all(&physical_root).unwrap();
        fs::write(physical_root.join("new-artifact"), "preserve\n").unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                physical_root.display().to_string(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "cargo".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: true,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(
            fs::read_to_string(physical_root.join("new-artifact")).unwrap(),
            "preserve\n"
        );
        assert!(physical_install.exists());
        assert!(
            fs::symlink_metadata(logical_install)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_archive_transition_preserves_root_behind_regular_launcher() {
        let dir = temp_dir("archive-regular-launcher");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let install_root = roots.install_dir.join("owner/tool");
        let public = roots.bin_dir.join("tool");
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::create_dir_all(roots.state_dir.join("owner")).unwrap();
        fs::write(install_root.join("tool"), "archive binary").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&roots.install_dir, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        fs::write(&public, "user launcher").unwrap();
        link_state::write(
            &link_state::path(&roots.state_dir, "owner/tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();
        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        cleanup_snapshot_for_test(&new_entry, &transition, &roots).unwrap();

        assert_eq!(fs::read_to_string(&public).unwrap(), "user launcher");
        assert_eq!(
            fs::read_to_string(install_root.join("tool")).unwrap(),
            "archive binary"
        );
    }

    #[test]
    #[cfg(unix)]
    fn repo_to_archive_transition_uses_locked_root_after_install_alias_retarget() {
        let dir = temp_dir("archive-locked-root-retarget");
        let physical_a = dir.join("physical-a");
        let physical_b = dir.join("physical-b");
        let logical_install = dir.join("install-link");
        fs::create_dir_all(&physical_a).unwrap();
        fs::create_dir_all(&physical_b).unwrap();
        symlink(&physical_a, &logical_install).unwrap();

        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: logical_install.clone(),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let locked_root = physical_a.join("owner/tool");
        fs::create_dir_all(&locked_root).unwrap();
        fs::create_dir_all(&roots.bin_dir).unwrap();
        fs::write(locked_root.join("tool"), "archive binary\n").unwrap();
        fs::write(
            github_release_install::archive_layout_path(&physical_a, "owner/tool"),
            "v1 archive\n",
        )
        .unwrap();
        let public = roots.bin_dir.join("tool");
        fs::write(&public, "user launcher\n").unwrap();

        let transition = Transition {
            old: ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                locked_root.to_string_lossy(),
            ),
            bin_links: Vec::new(),
            extra_links: Vec::new(),
            archive_state: ArchiveState::None,
            archive_root_identity: None,
            cleanup_evidence: crate::cleanup::Evidence::default(),
        };
        let new_entry = Entry {
            name: "owner/tool".to_owned(),
            method: "github:release".to_owned(),
            cmd: "tool".to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        };

        fs::remove_file(&logical_install).unwrap();
        symlink(&physical_b, &logical_install).unwrap();

        cleanup_snapshot(&new_entry, &transition, &roots, Some(&locked_root)).unwrap();

        assert_eq!(
            fs::read_to_string(locked_root.join("tool")).unwrap(),
            "archive binary\n"
        );
        assert_eq!(fs::read_to_string(public).unwrap(), "user launcher\n");
        assert!(!physical_b.join("owner/tool").try_exists().unwrap());
        assert_eq!(fs::read_link(logical_install).unwrap(), physical_b);
    }

    #[test]
    #[cfg(unix)]
    fn raw_release_transition_commits_manifest_after_atomic_publication() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("manifest-last-success");

        let item = install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            },
        )
        .unwrap();

        assert!(item.changed);
        assert_eq!(fs::read_link(&public).unwrap(), source);
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            entry.method
        );
        assert_eq!(fs::read_dir(&roots.bin_dir).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn raw_release_transition_rolls_back_when_real_manifest_commit_fails() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("manifest-commit-failure");
        let old_bytes = fs::read(&public).unwrap();

        let error = install_with_prepared_and_commit(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            },
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected manifest commit failure",
                )
                .into())
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected manifest commit failure")
        );
        assert_eq!(fs::read(&public).unwrap(), old_bytes);
        assert!(
            fs::symlink_metadata(public_transition_path(&public).unwrap()).is_err(),
            "rollback must retire its durable publication journal"
        );
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            crate::method::GITHUB_RELEASE
        );
    }

    #[test]
    #[cfg(unix)]
    fn interrupted_publication_rolls_back_or_commits_from_manifest_state() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("public-journal-recovery");
        let new = ManifestEntry::new(
            &entry.name,
            &entry.method,
            &entry.cmd,
            source.to_string_lossy(),
        );
        let old_bytes = fs::read(&public).unwrap();

        begin_public_transition(&transition, new.clone(), &roots, &manifest_path).unwrap();
        assert_eq!(fs::read_link(&public).unwrap(), source);
        recover_public_transition(&manifest_path, &public, &roots).unwrap();
        assert_eq!(fs::read(&public).unwrap(), old_bytes);
        assert!(fs::symlink_metadata(public_transition_path(&public).unwrap()).is_err());
        assert_eq!(fs::read_dir(&roots.bin_dir).unwrap().count(), 1);

        let manifest = crate::manifest::read(&manifest_path).unwrap();
        let refreshed = by_name(&manifest, std::slice::from_ref(&entry), &roots)
            .unwrap()
            .remove(&entry.name)
            .unwrap();
        begin_public_transition(&refreshed, new.clone(), &roots, &manifest_path).unwrap();
        crate::manifest::upsert(&manifest_path, new).unwrap();
        recover_public_transition(&manifest_path, &public, &roots).unwrap();
        assert_eq!(fs::read_link(&public).unwrap(), source);
        assert!(fs::symlink_metadata(public_transition_path(&public).unwrap()).is_err());
        assert_eq!(fs::read_dir(&roots.bin_dir).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn public_transition_never_clobbers_foreign_recovery_record() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("public-record-collision");
        let journal = public_transition_path(&public).unwrap();
        fs::write(&journal, b"foreign\n").unwrap();
        let old_bytes = fs::read(&public).unwrap();

        let error = install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("malformed public command transition")
        );
        assert_eq!(fs::read(&journal).unwrap(), b"foreign\n");
        assert_eq!(fs::read(&public).unwrap(), old_bytes);
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            crate::method::GITHUB_RELEASE
        );
    }

    #[test]
    #[cfg(unix)]
    fn ambiguous_release_transition_rejects_before_installer_runs() {
        let dir = temp_dir("ambiguous-preflight");
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        let public = roots.bin_dir.join("tool");
        let root = roots.install_dir.join("owner/tool");
        write_executable(&public, b"old release");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("sentinel"), b"preserve").unwrap();
        let manifest_path = roots.state_dir.join("manifest");
        crate::manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                crate::method::GITHUB_RELEASE,
                "tool",
                public.to_string_lossy(),
            ),
        )
        .unwrap();
        let entry = parse_entry("owner/tool|cargo|tool|-|-", Some("apt"));
        let manifest = crate::manifest::read(&manifest_path).unwrap();
        let mut transitions = by_name(&manifest, std::slice::from_ref(&entry), &roots).unwrap();
        let transition = transitions.remove(&entry.name).unwrap();
        let called = std::cell::Cell::new(false);

        let error =
            install_with_prepared(&entry, Some(&transition), &roots, &manifest_path, |_| {
                called.set(true);
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            })
            .unwrap_err();

        assert!(error.to_string().contains("ambiguous legacy release"));
        assert!(!called.get());
        assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"preserve");
    }

    #[test]
    #[cfg(unix)]
    fn raw_release_transition_preserves_replacement_written_after_snapshot() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("manifest-last-replacement");
        let replacement = roots.bin_dir.join("replacement");

        let item = install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                write_executable(&replacement, b"replacement");
                fs::rename(&replacement, &public)?;
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            },
        )
        .unwrap();

        assert_eq!(item.status, crate::update::ItemStatus::Warning);
        assert!(item.detail.contains("public command changed"));
        assert_eq!(fs::read(&public).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&roots.bin_dir).unwrap().count(), 1);
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            entry.method
        );

        let manifest = crate::manifest::read(&manifest_path).unwrap();
        assert!(
            by_name(&manifest, std::slice::from_ref(&entry), &roots)
                .unwrap()
                .is_empty(),
            "the preserved replacement must not be reclassified as the old release on retry"
        );
        install_with_prepared(&entry, None, &roots, &manifest_path, |_| {
            Ok(Item::current(
                entry.name.clone(),
                ItemReason::Fresh,
                "fresh",
            ))
        })
        .unwrap();
        assert_eq!(fs::read(&public).unwrap(), b"replacement");
    }

    #[test]
    #[cfg(unix)]
    fn raw_release_transition_preserves_directory_written_after_snapshot() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("manifest-last-directory");

        let item = install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                fs::remove_file(&public)?;
                fs::create_dir(&public)?;
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::changed(
                    entry.name.clone(),
                    ItemReason::Installed,
                    "installed",
                ))
            },
        )
        .unwrap();

        assert_eq!(item.status, crate::update::ItemStatus::Warning);
        assert!(public.is_dir());
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            entry.method
        );
    }

    #[test]
    #[cfg(unix)]
    fn failed_raw_release_transition_preserves_installer_replacement() {
        let (roots, manifest_path, entry, transition, public, source) =
            raw_release_transition("manifest-last-failure");
        let replacement = roots.bin_dir.join("replacement");

        let item = install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                write_executable(&replacement, b"replacement");
                fs::rename(&replacement, &public)?;
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::failed(
                    entry.name.clone(),
                    ItemReason::InstallFailed,
                    "failed",
                ))
            },
        )
        .unwrap();

        assert!(item.failed);
        assert_eq!(fs::read(&public).unwrap(), b"replacement");
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            crate::method::GITHUB_RELEASE
        );
    }

    #[test]
    #[cfg(unix)]
    fn raw_release_transition_recovers_publication_before_manifest_commit() {
        let (roots, manifest_path, entry, _transition, public, source) =
            raw_release_transition("manifest-last-crash");
        fs::remove_file(&public).unwrap();
        symlink(&source, &public).unwrap();
        let manifest = crate::manifest::read(&manifest_path).unwrap();
        let mut transitions = by_name(&manifest, std::slice::from_ref(&entry), &roots).unwrap();
        let transition = transitions.remove(&entry.name).unwrap();

        install_with_prepared(
            &entry,
            Some(&transition),
            &roots,
            &manifest_path,
            |prepared| {
                crate::manifest::upsert(
                    prepared,
                    ManifestEntry::new(
                        &entry.name,
                        &entry.method,
                        &entry.cmd,
                        source.to_string_lossy(),
                    ),
                )?;
                Ok(Item::current(
                    entry.name.clone(),
                    ItemReason::Fresh,
                    "fresh",
                ))
            },
        )
        .unwrap();

        assert_eq!(fs::read_link(&public).unwrap(), source);
        assert_eq!(
            crate::manifest::read(&manifest_path)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            entry.method
        );
    }

    #[test]
    #[cfg(unix)]
    fn prepared_manifest_never_clobbers_existing_candidate() {
        let (roots, manifest_path, entry, transition, _public, _source) =
            raw_release_transition("manifest-stage-collision");
        let collision = roots.state_dir.join(format!(
            ".manifest.shdeps-transition.{}.41",
            std::process::id()
        ));
        fs::write(&collision, b"foreign").unwrap();
        let mut nonces = [41, 42].into_iter();

        let prepared =
            prepare_manifest_with_nonce(&entry, Some(&transition), &roots, &manifest_path, || {
                nonces.next().unwrap()
            })
            .unwrap()
            .unwrap();

        assert_eq!(fs::read(&collision).unwrap(), b"foreign");
        assert_eq!(
            crate::manifest::read(&prepared)
                .unwrap()
                .get(&entry.name)
                .unwrap()
                .method,
            crate::method::GITHUB_RELEASE
        );
        assert!(prepared.ends_with(format!(
            ".manifest.shdeps-transition.{}.42",
            std::process::id()
        )));
    }

    #[cfg(unix)]
    fn raw_release_transition(name: &str) -> (Roots, PathBuf, Entry, Transition, PathBuf, PathBuf) {
        let dir = temp_dir(name);
        let roots = Roots {
            conf_dir: dir.join("conf"),
            hooks_dir: dir.join("hooks"),
            state_dir: dir.join("state"),
            git_dev_dir: dir.join("git-dev"),
            install_dir: dir.join("install"),
            bin_dir: dir.join("bin"),
            home: dir.join("home"),
        };
        fs::create_dir_all(&roots.bin_dir).unwrap();
        let public = roots.bin_dir.join("tool");
        write_executable(&public, b"old release");
        let source = roots.install_dir.join("owner/tool/bin/tool");
        let manifest_path = roots.state_dir.join("manifest");
        crate::manifest::upsert(
            &manifest_path,
            ManifestEntry::new(
                "owner/tool",
                crate::method::GITHUB_RELEASE,
                "tool",
                public.to_string_lossy(),
            ),
        )
        .unwrap();
        let entry = parse_entry("owner/tool|cargo|tool|-|-", Some("apt"));
        let manifest = crate::manifest::read(&manifest_path).unwrap();
        let mut transitions = by_name(&manifest, std::slice::from_ref(&entry), &roots).unwrap();
        let transition = transitions.remove(&entry.name).unwrap();
        write_executable(&source, b"new provider");
        (roots, manifest_path, entry, transition, public, source)
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-transition-{name}"))
    }
}
