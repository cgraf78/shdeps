//! Ownership-aware cleanup for prune and method transitions.
//!
//! Bash historically had separate call paths for orphan pruning and method
//! transitions, but both paths make the same ownership decision: remove only
//! artifacts that shdeps created or explicitly tracks, and leave system
//! packages, local development clones, and user-owned files alone. Keeping the
//! built-in cleanup rules here gives the Rust updater one place to preserve
//! those safety boundaries.

use std::fs;
#[cfg(unix)]
use std::hash::{Hash, Hasher};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Result;
use crate::config;
use crate::github_release_install::{self, ArchiveState};
use crate::link_state::{self, Kind};
use crate::manifest::{Manifest, ManifestEntry};
use crate::method;

#[cfg(unix)]
static UNLINK_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone)]
struct LogicalBaseIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    kind: LogicalBaseKind,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogicalBaseKind {
    Directory,
    Symlink(PathBuf),
}

#[derive(Debug, Clone)]
struct ManagedRootIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    kind: ManagedRootKind,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    tree_fingerprint: Option<u64>,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagedRootKind {
    Directory,
    Symlink(PathBuf),
}

/// Filesystem roots needed for built-in artifact cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// State directory containing stamps, `.links`, and `.binlinks`.
    pub state_dir: PathBuf,
    /// Managed install root, normally `~/.local/share`.
    pub install_dir: PathBuf,
    /// Public command directory, normally `~/.local/bin`.
    pub bin_dir: PathBuf,
}

/// Summary of built-in cleanup decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Paths removed by the built-in cleanup rules.
    pub removed: Vec<PathBuf>,
    /// True when a package-manager dependency was intentionally preserved.
    pub preserved_package: bool,
    /// True when a custom dependency needs hook/manual cleanup outside this layer.
    pub custom_requires_hook: bool,
}

/// Filesystem identity captured before arbitrary cleanup hooks can mutate paths.
#[derive(Debug, Clone, Default)]
pub(crate) struct Evidence {
    public_regular_identity: Option<FileIdentity>,
    logical_install_base_identity: Option<LogicalBaseIdentity>,
    logical_install_base_was_absent: bool,
    physical_install_base: Option<PathBuf>,
    physical_install_base_identity: Option<DirectoryIdentity>,
    managed_install_root: Option<PathBuf>,
    managed_install_root_identity: Option<ManagedRootIdentity>,
}

impl Evidence {
    pub(crate) fn public_regular_identity(&self) -> Option<&FileIdentity> {
        self.public_regular_identity.as_ref()
    }

    pub(crate) fn managed_install_root(&self) -> Option<&Path> {
        self.managed_install_root.as_deref()
    }

    pub(crate) fn physical_install_base(&self) -> Option<&Path> {
        self.physical_install_base.as_deref()
    }

    pub(crate) fn install_base_matches(&self) -> bool {
        #[cfg(unix)]
        {
            self.physical_install_base
                .as_deref()
                .zip(self.physical_install_base_identity.as_ref())
                .is_some_and(|(path, identity)| identity.matches(path))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub(crate) fn managed_root_matches(&self) -> bool {
        #[cfg(unix)]
        {
            self.managed_install_root
                .as_deref()
                .zip(self.managed_install_root_identity.as_ref())
                .is_some_and(|(path, identity)| identity.matches_before_removal(path))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub(crate) fn managed_root_link_authority(&self) -> bool {
        #[cfg(unix)]
        {
            if let Some(identity) = self.managed_install_root_identity.as_ref() {
                return self
                    .managed_install_root
                    .as_deref()
                    .is_some_and(|path| identity.matches_before_removal(path));
            }
            self.managed_install_root.as_deref().is_some_and(|path| {
                fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            })
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub(crate) fn logical_base_matches(&self, install_dir: &Path) -> bool {
        match self.logical_install_base_identity.as_ref() {
            Some(identity) => {
                identity.matches(install_dir)
                    || fs::symlink_metadata(install_dir)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }
            None if self.logical_install_base_was_absent => fs::symlink_metadata(install_dir)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            None => false,
        }
    }

    pub(crate) fn owner_roots(&self, install_dir: &Path, logical_root: PathBuf) -> Vec<PathBuf> {
        if !self.managed_root_link_authority() {
            return Vec::new();
        }
        let mut roots = Vec::new();
        if self.logical_base_matches(install_dir) {
            roots.push(logical_root);
        }
        if self.install_base_matches()
            && let Some(root) = self.managed_install_root.as_ref()
            && !roots.contains(root)
        {
            roots.push(root.clone());
        }
        roots
    }

    pub(crate) fn authorizes_managed_root(&self, install_dir: &Path, expected_root: &Path) -> bool {
        self.managed_install_root.as_deref() == Some(expected_root)
            && self.managed_root_link_authority()
            && (self.install_base_matches() || self.logical_base_matches(install_dir))
    }

    pub(crate) fn remove_managed_root(&self) -> Result<bool> {
        #[cfg(unix)]
        {
            let (Some(root), Some(identity)) = (
                self.managed_install_root.as_ref(),
                self.managed_install_root_identity.as_ref(),
            ) else {
                return Ok(false);
            };
            if !self.install_base_matches() || !self.managed_root_matches() {
                return Ok(false);
            }
            remove_owned_managed_root(root, identity, &mut Summary::default())
        }
        #[cfg(not(unix))]
        {
            Ok(false)
        }
    }
}

impl Summary {
    fn note_removed(&mut self, path: impl Into<PathBuf>) {
        self.removed.push(path.into());
    }
}

/// Returns manifest entries whose installed method differs from current config.
#[must_use]
pub fn method_transitions(
    manifest: &Manifest,
    config: &[crate::config::Entry],
) -> Vec<ManifestEntry> {
    config
        .iter()
        .filter_map(|entry| {
            let installed = manifest.get(&entry.name)?;
            (installed.method != entry.method).then(|| installed.clone())
        })
        .collect()
}

/// Removes built-in artifacts for one manifest entry.
///
/// Hook `uninstall()` execution intentionally lives outside this function. Hooks
/// can run arbitrary shell, so coordinators run them outside the non-reentrant
/// checkout lock and acquire that narrower lock only for deterministic filesystem
/// cleanup. The broader Shdeps state lock remains held across the full update or
/// prune transaction.
pub fn remove_builtin(entry: &ManifestEntry, roots: &Roots) -> Result<Summary> {
    if entry.method == method::GITHUB_REPO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo cleanup requires an acquired checkout-lock root",
        )
        .into());
    }
    remove_builtin_with_repo_root(entry, roots, None)
}

/// Removes built-in artifacts while honoring an already locked repo root.
///
/// Coordinators pass the normalized path yielded by checkout-lock acquisition
/// so a configured install-root symlink cannot be retargeted between locking
/// and cleanup. Non-repository methods ignore this argument.
pub(crate) fn remove_builtin_with_repo_root(
    entry: &ManifestEntry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
) -> Result<Summary> {
    remove_builtin_with_protection(entry, roots, locked_repo_root, false)
}

/// Removes built-in artifacts while preserving a proven surviving raw command.
pub(crate) fn remove_builtin_with_protection(
    entry: &ManifestEntry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
    preserve_regular_public: bool,
) -> Result<Summary> {
    let evidence = capture_evidence(entry, roots)?;
    remove_builtin_with_evidence(
        entry,
        roots,
        locked_repo_root,
        preserve_regular_public,
        evidence,
    )
}

/// Captures generation identity before an arbitrary uninstall hook can replace it.
pub(crate) fn capture_evidence(entry: &ManifestEntry, roots: &Roots) -> Result<Evidence> {
    let public_regular_identity = if entry.method == method::GITHUB_RELEASE {
        regular_file_identity(&roots.bin_dir.join(&entry.cmd))?
    } else {
        None
    };
    let captures_install_root = entry.method == method::GITHUB_REPO
        || method::is_external(&entry.method)
        || (entry.method == method::GITHUB_RELEASE
            && github_release_install::archive_state(
                &roots.state_dir,
                &roots.install_dir,
                &roots.bin_dir.join(&entry.cmd),
                &entry.name,
            )
            .is_ok_and(|state| state == ArchiveState::Proven));
    let (logical_install_base_identity, logical_install_base_was_absent) = if captures_install_root
    {
        match logical_base_identity(&roots.install_dir)? {
            Some(identity) => (Some(identity), false),
            None => (None, true),
        }
    } else {
        (None, false)
    };
    let physical_install_base = if captures_install_root {
        match fs::canonicalize(&roots.install_dir) {
            Ok(base) => Some(base),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let physical_install_base_identity = physical_install_base
        .as_ref()
        .map(|base| directory_identity(base))
        .transpose()?
        .flatten();
    let managed_install_root = captures_install_root.then(|| {
        let name = if entry.method == method::GITHUB_REPO {
            config::canonical_name(&entry.name, method::GITHUB_REPO)
        } else {
            entry.name.clone()
        };
        physical_install_base
            .as_ref()
            .unwrap_or(&roots.install_dir)
            .join(name)
    });
    let managed_install_root_identity = managed_install_root
        .as_ref()
        .map(|root| managed_root_identity(root))
        .transpose()?
        .flatten();
    Ok(Evidence {
        public_regular_identity,
        logical_install_base_identity,
        logical_install_base_was_absent,
        physical_install_base,
        physical_install_base_identity,
        managed_install_root,
        managed_install_root_identity,
    })
}

/// Removes built-in artifacts using identity captured before caller-owned hooks.
pub(crate) fn remove_builtin_with_evidence(
    entry: &ManifestEntry,
    roots: &Roots,
    locked_repo_root: Option<&Path>,
    preserve_regular_public: bool,
    evidence: Evidence,
) -> Result<Summary> {
    if entry.method == method::GITHUB_REPO && locked_repo_root.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "github:repo cleanup requires an acquired checkout-lock root",
        )
        .into());
    }
    let mut summary = Summary::default();

    match entry.method.as_str() {
        method::PKG => {
            // System packages are explicitly not owned by shdeps. Migrating a
            // dependency from `pkg` to another method must clear shdeps tracking
            // later, but uninstalling the OS package would be surprising and
            // potentially destructive.
            summary.preserved_package = true;
            let proof = crate::package_proof::path(&roots.state_dir, &entry.name);
            let had_proof = proof.exists();
            crate::package_proof::remove(&roots.state_dir, &entry.name)?;
            if had_proof {
                summary.note_removed(proof);
            }
        }
        method::GITHUB_REPO => {
            let repo_root = locked_repo_root.expect("validated above");
            let root_authority = evidence.authorizes_managed_root(&roots.install_dir, repo_root);
            if !root_authority {
                return Err(std::io::Error::other(format!(
                    "repository root changed after cleanup evidence was captured: {}",
                    repo_root.display()
                ))
                .into());
            }
            let logical_root = roots
                .install_dir
                .join(config::canonical_name(&entry.name, method::GITHUB_REPO));
            let mut owner_roots = vec![repo_root.to_path_buf()];
            if evidence.logical_base_matches(&roots.install_dir) {
                owner_roots.push(logical_root);
            }
            unlink_state(roots, &entry.name, Kind::Bin, &owner_roots, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, &owner_roots, &mut summary)?;
            if let Some(path) = remove_legacy_repo_command(entry, roots, &owner_roots)? {
                summary.note_removed(path);
            }

            // Human-editable state may be missing or point at another managed
            // dependency. Cleanup authority comes only from the normalized
            // root yielded by the checkout lock.
            #[cfg(unix)]
            if let Some(identity) = evidence.managed_install_root_identity.as_ref() {
                remove_owned_managed_root(repo_root, identity, &mut summary)?;
            }

            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        binary if method::is_binary_install_root(binary) => {
            let public_bin = roots.bin_dir.join(&entry.cmd);
            let logical_root = roots.install_dir.join(&entry.name);
            let physical_root = evidence.managed_install_root.as_ref();
            let physical_base = evidence.physical_install_base.as_ref();
            let root_authority = evidence.install_base_matches() && evidence.managed_root_matches();
            let mut owner_roots = Vec::new();
            if root_authority {
                if evidence.logical_base_matches(&roots.install_dir) {
                    owner_roots.push(logical_root);
                }
                if let Some(physical_root) = physical_root {
                    owner_roots.push(physical_root.clone());
                }
            }
            let archive_state = if binary == method::GITHUB_RELEASE {
                match physical_base.filter(|_| evidence.install_base_matches()) {
                    Some(physical_base) => github_release_install::archive_state(
                        &roots.state_dir,
                        physical_base,
                        &public_bin,
                        &entry.name,
                    )?,
                    None => ArchiveState::None,
                }
            } else {
                ArchiveState::None
            };
            let preserve_public_launcher = binary == method::GITHUB_RELEASE
                && archive_state != ArchiveState::None
                && github_release_install::is_non_symlink(&public_bin);
            let public_regular_identity = if binary == method::GITHUB_RELEASE
                && !preserve_public_launcher
                && !preserve_regular_public
            {
                evidence.public_regular_identity
            } else {
                None
            };

            unlink_state(roots, &entry.name, Kind::Bin, &owner_roots, &mut summary)?;
            unlink_state(roots, &entry.name, Kind::Extras, &owner_roots, &mut summary)?;
            if !preserve_public_launcher {
                let mut removed_public = unlink_owned_symlink(&public_bin, &owner_roots)?;
                if !removed_public && binary == method::GITHUB_RELEASE && !preserve_regular_public {
                    if let Some(identity) = public_regular_identity.as_ref() {
                        removed_public = remove_owned_regular_file(&public_bin, identity)?;
                    }
                }
                if removed_public {
                    summary.note_removed(public_bin);
                }
            }

            if binary != method::GITHUB_RELEASE || archive_state == ArchiveState::Proven {
                #[cfg(unix)]
                if root_authority {
                    if let (Some(physical_root), Some(physical_base), Some(identity)) = (
                        physical_root,
                        physical_base,
                        evidence.managed_install_root_identity.as_ref(),
                    ) {
                        if remove_owned_managed_root(physical_root, identity, &mut summary)? {
                            remove_empty_install_parents(
                                physical_root,
                                physical_base,
                                &mut summary,
                            )?;
                        }
                    }
                }
            }
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        method::CUSTOM => {
            // Custom deps have no built-in ownership model. The hook runner owns
            // `uninstall()`; this function only clears shdeps' own stamps so a
            // future reinstall does not inherit stale remote/cache state.
            summary.custom_requires_hook = true;
            remove_stamps(&roots.state_dir, &entry.name, &mut summary)?;
        }
        _ => {}
    }

    Ok(summary)
}

/// Removes only a legacy repo command symlink whose target proves Shdeps ownership.
///
/// Modern installs record every public command in `.binlinks`, so the normal
/// tracked unlink above is authoritative. This fallback exists for older state
/// that predated that ledger. A regular file is always unowned, and a symlink
/// is removed only when its literal target is the expected command under the
/// configured logical checkout or the checkout-lock-normalized physical root.
/// That rule preserves generated client adapters and foreign symlinks during
/// prune or method transitions without trusting human-editable manifest paths.
pub(crate) fn remove_legacy_repo_command(
    entry: &ManifestEntry,
    roots: &Roots,
    owner_roots: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let canonical_name = config::canonical_name(&entry.name, method::GITHUB_REPO);
    let short = config::short_name(&canonical_name);
    let public = roots.bin_dir.join(short);
    let metadata = match fs::symlink_metadata(&public) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let target = fs::read_link(&public)?;
    let expected = owner_roots
        .iter()
        .map(|root| root.join("bin").join(short))
        .collect::<Vec<_>>();
    if !expected.contains(&target) {
        return Ok(None);
    }
    Ok(unlink_symlink_with_exact_target(&public, &expected)?.then_some(public))
}

/// Removes TTL and revision stamps for a dependency name.
pub fn remove_stamps(state_dir: &Path, name: &str, summary: &mut Summary) -> Result<()> {
    let stamp_dir = state_dir
        .join(name)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.to_path_buf());
    let Some(base_name) = name.rsplit('/').next() else {
        return Ok(());
    };

    let Ok(entries) = fs::read_dir(&stamp_dir) else {
        return Ok(());
    };

    for entry in entries {
        let path = entry?.path();
        let Some(file_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
            continue;
        };
        // All known stamp kinds are single words without dots (repo, release,
        // github, cargo, etc.). Requiring a dot-free middle prevents
        // accidentally matching stamps for a dep whose name starts with the
        // same prefix, e.g. `tool.extra.repo.stamp` when base_name is `tool`.
        let is_stamp = file_name
            .strip_prefix(&format!("{base_name}."))
            .and_then(|s| s.strip_suffix(".stamp"))
            .is_some_and(|kind| !kind.contains('.'));
        let is_rev = file_name == format!("{base_name}.rev");
        if is_stamp || is_rev {
            remove_file_if_present(&path, summary)?;
        }
    }
    Ok(())
}

fn unlink_state(
    roots: &Roots,
    name: &str,
    kind: Kind,
    owner_roots: &[PathBuf],
    summary: &mut Summary,
) -> Result<()> {
    let state_path = link_state::path(&roots.state_dir, name, kind);
    let had_state = state_path.exists();
    let removed = link_state::unlink_tracked_matching(&state_path, |link| {
        unlink_owned_symlink(link, owner_roots)
    })?;

    for link in removed {
        summary.note_removed(link);
    }
    if had_state {
        summary.note_removed(state_path);
    }
    Ok(())
}

pub(crate) fn symlink_targets_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(resolved) = immediate_symlink_target(path) else {
        return false;
    };
    roots
        .iter()
        .map(|root| lexical_normalize(root))
        .any(|root| resolved.starts_with(root))
}

fn symlink_targets_any_exact_path(path: &Path, expected: &[PathBuf]) -> bool {
    let Some(target) = immediate_symlink_target(path) else {
        return false;
    };
    expected
        .iter()
        .map(|path| lexical_normalize(path))
        .any(|path| target == path)
}

fn immediate_symlink_target(path: &Path) -> Option<PathBuf> {
    let target = fs::read_link(path).ok()?;
    let resolved = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
    Some(lexical_normalize(&resolved))
}

pub(crate) fn unlink_owned_symlink(path: &Path, roots: &[PathBuf]) -> Result<bool> {
    if !symlink_targets_any_root(path, roots) {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        quarantine_matching_with(
            path,
            |claimed| symlink_targets_any_root(claimed, roots),
            crate::repo_transition::rename_noreplace,
        )
    }

    #[cfg(not(unix))]
    {
        if symlink_targets_any_root(path, roots) {
            fs::remove_file(path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn unlink_symlink_with_exact_target(path: &Path, expected: &[PathBuf]) -> Result<bool> {
    if !symlink_targets_any_exact_path(path, expected) {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        quarantine_matching_with(
            path,
            |claimed| symlink_targets_any_exact_path(claimed, expected),
            crate::repo_transition::rename_noreplace,
        )
    }

    #[cfg(not(unix))]
    {
        if symlink_targets_any_exact_path(path, expected) {
            fs::remove_file(path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(unix)]
fn quarantine_matching_with(
    path: &Path,
    mut owned: impl FnMut(&Path) -> bool,
    mut rename_noreplace: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tracked link has no parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tracked link has no file name",
        )
    })?;

    for _ in 0..16 {
        let nonce = UNLINK_NONCE.fetch_add(1, Ordering::Relaxed);
        let mut quarantine_name = std::ffi::OsString::from(".");
        quarantine_name.push(file_name);
        quarantine_name.push(format!(".shdeps-unlink.{}.{}", std::process::id(), nonce));
        let quarantine = parent.join(quarantine_name);

        match rename_noreplace(path, &quarantine) {
            Ok(()) => {
                if owned(&quarantine) {
                    fs::remove_file(&quarantine)?;
                    return Ok(true);
                }
                match rename_noreplace(&quarantine, path) {
                    Ok(()) => return Ok(false),
                    Err(error) => {
                        return Err(std::io::Error::other(format!(
                            "tracked link changed during cleanup; preserved replacement at {} after restore failed: {error}",
                            quarantine.display()
                        ))
                        .into());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::other(format!(
        "could not allocate cleanup quarantine beside {}",
        path.display()
    ))
    .into())
}

#[cfg(unix)]
fn remove_owned_managed_root(
    path: &Path,
    identity: &ManagedRootIdentity,
    summary: &mut Summary,
) -> Result<bool> {
    if !identity.matches_before_removal(path) {
        return Ok(false);
    }
    // The caller holds the Shdeps state lock and has already run any arbitrary
    // uninstall hook before this final generation check. Delete the stable path
    // directly so a process death cannot strand the whole install tree under a
    // hidden quarantine name. Non-cooperating writers outside Shdeps' advisory
    // lock remain outside the state-lock threat model.
    match identity.kind {
        ManagedRootKind::Directory => fs::remove_dir_all(path)?,
        ManagedRootKind::Symlink(_) => fs::remove_file(path)?,
    }
    summary.note_removed(path);
    Ok(true)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(not(unix))]
    length: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl PartialEq for FileIdentity {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device
                && self.inode == other.inode
                && self.length == other.length
                && self.modified_seconds == other.modified_seconds
                && self.modified_nanoseconds == other.modified_nanoseconds
                && self.changed_seconds == other.changed_seconds
                && self.changed_nanoseconds == other.changed_nanoseconds
                && self.mode == other.mode
        }
        #[cfg(not(unix))]
        {
            self.length == other.length && self.modified == other.modified
        }
    }
}

impl Eq for FileIdentity {}

fn logical_base_identity(path: &Path) -> Result<Option<LogicalBaseIdentity>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Ok(None);
    }

    #[cfg(unix)]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(LogicalBaseIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: LogicalBaseKind::Directory,
        })),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Some(LogicalBaseIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: LogicalBaseKind::Symlink(fs::read_link(path)?),
        })),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "managed install base is neither a directory nor a symlink: {}",
                path.display()
            ),
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Ok(None);
    }

    #[cfg(unix)]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn managed_root_identity(path: &Path) -> Result<Option<ManagedRootIdentity>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Ok(None);
    }

    #[cfg(unix)]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(Some(ManagedRootIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: ManagedRootKind::Directory,
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            tree_fingerprint: Some(tree_fingerprint(path)?),
        })),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Some(ManagedRootIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: ManagedRootKind::Symlink(fs::read_link(path)?),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            tree_fingerprint: None,
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
impl DirectoryIdentity {
    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }
}

#[cfg(unix)]
impl LogicalBaseIdentity {
    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            if metadata.dev() != self.device || metadata.ino() != self.inode {
                return false;
            }
            match &self.kind {
                LogicalBaseKind::Directory => metadata.file_type().is_dir(),
                LogicalBaseKind::Symlink(target) => {
                    metadata.file_type().is_symlink()
                        && fs::read_link(path).is_ok_and(|current| current == *target)
                }
            }
        })
    }
}

#[cfg(unix)]
impl ManagedRootIdentity {
    fn matches_before_removal(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            if metadata.dev() != self.device || metadata.ino() != self.inode {
                return false;
            }
            match &self.kind {
                ManagedRootKind::Directory => {
                    metadata.file_type().is_dir()
                        && metadata.mtime() == self.modified_seconds
                        && metadata.mtime_nsec() == self.modified_nanoseconds
                        && metadata.ctime() == self.changed_seconds
                        && metadata.ctime_nsec() == self.changed_nanoseconds
                        && metadata.mode() == self.mode
                        && tree_fingerprint(path).ok() == self.tree_fingerprint
                }
                ManagedRootKind::Symlink(target) => {
                    metadata.file_type().is_symlink()
                        && fs::read_link(path).is_ok_and(|current| current == *target)
                }
            }
        })
    }
}

#[cfg(unix)]
fn tree_fingerprint(root: &Path) -> Result<u64> {
    fn visit(root: &Path, dir: &Path, hasher: &mut impl Hasher) -> Result<()> {
        let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed-root entry escaped its captured root",
                )
            })?;
            relative.as_os_str().as_bytes().hash(hasher);
            let metadata = fs::symlink_metadata(&path)?;
            metadata.dev().hash(hasher);
            metadata.ino().hash(hasher);
            metadata.mode().hash(hasher);
            metadata.size().hash(hasher);
            metadata.mtime().hash(hasher);
            metadata.mtime_nsec().hash(hasher);
            metadata.ctime().hash(hasher);
            metadata.ctime_nsec().hash(hasher);
            if metadata.file_type().is_symlink() {
                fs::read_link(&path)?.as_os_str().as_bytes().hash(hasher);
            } else if metadata.file_type().is_dir() {
                visit(root, &path, hasher)?;
            }
        }
        Ok(())
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    visit(root, root, &mut hasher)?;
    Ok(hasher.finish())
}

pub(crate) fn regular_file_identity(path: &Path) -> Result<Option<FileIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            let identity = FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.size(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
                mode: metadata.mode(),
            };
            #[cfg(not(unix))]
            let identity = FileIdentity {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            };
            Ok(Some(identity))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn remove_owned_regular_file(path: &Path, identity: &FileIdentity) -> Result<bool> {
    #[cfg(unix)]
    {
        quarantine_regular_file_with(
            path,
            identity,
            true,
            crate::repo_transition::rename_noreplace,
        )
    }

    #[cfg(not(unix))]
    {
        if regular_file_identity(path)?.as_ref() == Some(identity) {
            fs::remove_file(path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn remove_owned_regular_file_after_rename(
    path: &Path,
    identity: &FileIdentity,
) -> Result<bool> {
    #[cfg(unix)]
    {
        quarantine_regular_file_with(
            path,
            identity,
            false,
            crate::repo_transition::rename_noreplace,
        )
    }

    #[cfg(not(unix))]
    {
        remove_owned_regular_file(path, identity)
    }
}

pub(crate) fn regular_file_matches_after_rename(
    path: &Path,
    identity: &FileIdentity,
) -> Result<bool> {
    match fs::symlink_metadata(path) {
        #[cfg(unix)]
        Ok(metadata) => Ok(identity.matches_after_rename(&metadata)),
        #[cfg(not(unix))]
        Ok(_) => Ok(regular_file_identity(path)?.as_ref() == Some(identity)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn quarantine_regular_file_with(
    path: &Path,
    identity: &FileIdentity,
    require_unchanged_ctime: bool,
    rename_noreplace: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool> {
    let owned = if require_unchanged_ctime {
        regular_file_identity(path)?.as_ref() == Some(identity)
    } else {
        regular_file_matches_after_rename(path, identity)?
    };
    if !owned {
        return Ok(false);
    }
    quarantine_matching_with(
        path,
        |claimed| regular_file_matches_after_rename(claimed, identity).unwrap_or(false),
        rename_noreplace,
    )
}

#[cfg(unix)]
impl FileIdentity {
    fn matches_after_rename(&self, metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_file()
            && self.device == metadata.dev()
            && self.inode == metadata.ino()
            && self.length == metadata.size()
            && self.modified_seconds == metadata.mtime()
            && self.modified_nanoseconds == metadata.mtime_nsec()
            && self.mode == metadata.mode()
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let absolute = path.is_absolute();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::ParentDir) | None if !absolute => {
                    normalized.push(component.as_os_str());
                }
                _ => {}
            },
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Returns the only checkout root a manifest row may authorize for locking.
///
/// Repository ownership is structural: a valid dependency name owns exactly
/// `<install_dir>/<name>`. The human-editable `install_path` is diagnostic
/// state, not authority to select a sibling dependency or arbitrary path.
/// Physicalizing the configured install root also keeps cleanup on the same
/// root the checkout lock serialized when that root contains a symlink.
pub(crate) fn safe_repo_root(entry: &ManifestEntry, roots: &Roots) -> Option<PathBuf> {
    let name = config::canonical_name(&entry.name, method::GITHUB_REPO);
    if !config::valid_dep_name(&name) {
        return None;
    }
    Some(physical_install_root(&roots.install_dir).join(name))
}

/// Resolves the configured install root once before ownership-sensitive work.
pub(crate) fn physical_install_root(install_dir: &Path) -> PathBuf {
    fs::canonicalize(install_dir).unwrap_or_else(|_| install_dir.to_path_buf())
}

/// Recovers the physical install-root boundary from one locked repo path.
pub(crate) fn install_root_for_repo(repo_root: &Path, name: &str) -> Option<PathBuf> {
    let name = config::canonical_name(name, method::GITHUB_REPO);
    if !config::valid_dep_name(&name) {
        return None;
    }
    let mut root = repo_root;
    for _ in Path::new(&name).components() {
        root = root.parent()?;
    }
    Some(root.to_path_buf())
}

/// Validates manifest fields before they select hooks or artifact paths.
///
/// The manifest is intentionally human-editable and older versions did not
/// validate rows while loading them. Mutation coordinators therefore enforce
/// the current config grammar immediately before using a saved name or command
/// in any filesystem path, while leaving malformed state intact for recovery.
pub(crate) fn validate_manifest_artifact_entry(entry: &ManifestEntry) -> Result<()> {
    if !config::valid_dep_name(&entry.name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe dependency name in manifest: {}", entry.name),
        )
        .into());
    }
    if !config::valid_cmd_basename(&entry.cmd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsafe command name in manifest: {}", entry.cmd),
        )
        .into());
    }
    Ok(())
}

fn remove_file_if_present(path: &Path, summary: &mut Summary) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => summary.note_removed(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_empty_install_parents(
    path: &Path,
    install_dir: &Path,
    summary: &mut Summary,
) -> Result<()> {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == install_dir || dir == Path::new("/") {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => summary.note_removed(dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                // Parent cleanup is best-effort by design. These directories
                // commonly contain sibling deps (`github.com/owner/*`), and the
                // Bash implementation stops at the first `rmdir` failure. Keep
                // that safety bias here rather than turning a successfully
                // removed dependency into a hard failure because an ancestor was
                // non-empty or otherwise not removable.
                break;
            }
        }
        parent = dir.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, symlink};

    #[cfg(unix)]
    use super::quarantine_matching_with;
    use super::{
        Roots, Summary, lexical_normalize, method_transitions, remove_builtin_with_repo_root,
        remove_stamps, safe_repo_root, symlink_targets_any_root,
    };
    use crate::config::Entry;
    use crate::github_release_install;
    use crate::link_state::{self, Kind};
    use crate::manifest::{Manifest, ManifestEntry};

    fn remove_for_test(entry: &ManifestEntry, roots: &Roots) -> crate::Result<Summary> {
        let repo_root = (entry.method == crate::method::GITHUB_REPO)
            .then(|| safe_repo_root(entry, roots))
            .flatten();
        remove_builtin_with_repo_root(entry, roots, repo_root.as_deref())
    }

    #[test]
    fn method_transitions_ignore_orphans_and_return_method_changes() {
        let manifest = Manifest::parse(
            "repo|github:repo|repo|/tmp/repo\npkg|pkg|pkg|\norphan|cargo|orphan|/tmp/orphan\n",
        );
        let config = vec![entry("repo", "github:repo"), entry("pkg", "github:repo")];

        assert_eq!(
            method_transitions(&manifest, &config),
            vec![ManifestEntry::new("pkg", "pkg", "pkg", "")]
        );
    }

    #[test]
    fn lexical_normalization_preserves_unresolved_parent_components() {
        assert_eq!(
            lexical_normalize(Path::new("../../managed/../tool")),
            PathBuf::from("../../tool")
        );
    }

    #[test]
    #[cfg(unix)]
    fn immediate_target_ownership_handles_relative_dangling_and_sibling_paths() {
        let fixture = Fixture::new("immediate-targets");
        let managed = fixture.roots.install_dir.join("managed");
        let sibling = fixture.roots.install_dir.join("managed-other");
        let public = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();

        symlink("../share/managed/bin/tool", &public).unwrap();
        assert!(symlink_targets_any_root(
            &public,
            std::slice::from_ref(&managed)
        ));

        fs::remove_file(&public).unwrap();
        symlink("../share/managed-other/bin/tool", &public).unwrap();
        assert!(!symlink_targets_any_root(
            &public,
            std::slice::from_ref(&managed)
        ));
        assert!(symlink_targets_any_root(
            &public,
            std::slice::from_ref(&sibling)
        ));

        fs::remove_file(&public).unwrap();
        symlink("../share/managed/../replacement/bin/tool", &public).unwrap();
        assert!(!symlink_targets_any_root(
            &public,
            std::slice::from_ref(&managed)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn owned_unlink_restores_replacement_swapped_before_atomic_claim() {
        let fixture = Fixture::new("unlink-race");
        let managed = fixture.roots.install_dir.join("managed");
        let target = managed.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "old command").unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&target, &public).unwrap();
        let mut swapped = false;

        let removed = quarantine_matching_with(
            &public,
            |claimed| symlink_targets_any_root(claimed, std::slice::from_ref(&managed)),
            |source, destination| {
                if source == public && !swapped {
                    swapped = true;
                    fs::remove_file(source)?;
                    fs::write(source, "replacement command")?;
                }
                crate::repo_transition::rename_noreplace(source, destination)
            },
        )
        .unwrap();

        assert!(!removed);
        assert_eq!(fs::read_to_string(public).unwrap(), "replacement command");
    }

    #[test]
    #[cfg(unix)]
    fn owned_regular_removal_restores_replacement_swapped_before_atomic_claim() {
        let fixture = Fixture::new("regular-race");
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write_bin("tool", "old command");
        let identity = super::regular_file_identity(&public).unwrap().unwrap();
        let mut swapped = false;

        let removed = quarantine_matching_with(
            &public,
            |claimed| {
                super::regular_file_identity(claimed)
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(&identity)
            },
            |source, destination| {
                if source == public && !swapped {
                    swapped = true;
                    fs::remove_file(source)?;
                    fs::write(source, "replacement command")?;
                }
                crate::repo_transition::rename_noreplace(source, destination)
            },
        )
        .unwrap();

        assert!(!removed);
        assert_eq!(fs::read_to_string(public).unwrap(), "replacement command");
    }

    #[test]
    fn github_repo_cleanup_rejects_missing_lock_authority() {
        let fixture = Fixture::new("repo-missing-lock-authority");
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");

        let error = super::remove_builtin(&entry, &fixture.roots).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires an acquired checkout-lock root")
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_removes_symlink_install_and_tracked_links_but_preserves_target() {
        let fixture = Fixture::new("repo-symlink");
        let target = fixture.dir.join("target");
        // The validated dependency name owns this exact install root. The
        // recorded path is retained only to prove that it cannot redirect
        // cleanup elsewhere.
        let install_link = fixture.roots.install_dir.join("repo-tool");
        let short_bin = fixture.roots.bin_dir.join("repo-tool");
        let extra_bin = fixture.roots.bin_dir.join("repo-extra");
        // Extras live wherever the linker placed them, but their immediate
        // targets still pass through the managed checkout root.
        let man_link = fixture.dir.join("home/.local/share/man/man1/repo-tool.1");
        fs::create_dir_all(target.join("bin")).unwrap();
        fs::write(target.join("bin/repo-tool"), "#!/bin/sh\n").unwrap();
        fs::create_dir_all(install_link.parent().unwrap()).unwrap();
        fs::create_dir_all(short_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(man_link.parent().unwrap()).unwrap();
        symlink(&target, &install_link).unwrap();
        symlink(install_link.join("bin/repo-tool"), &short_bin).unwrap();
        symlink(install_link.join("bin/repo-tool"), &extra_bin).unwrap();
        symlink(install_link.join("bin/repo-tool"), &man_link).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "repo-tool", Kind::Bin),
            std::slice::from_ref(&extra_bin),
        )
        .unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "repo-tool", Kind::Extras),
            std::slice::from_ref(&man_link),
        )
        .unwrap();
        fixture.write_state("repo-tool.repo.stamp", "1\n");

        remove_for_test(
            &ManifestEntry::new(
                "repo-tool",
                "github:repo",
                "repo-tool",
                install_link.to_string_lossy(),
            ),
            &fixture.roots,
        )
        .unwrap();

        assert!(target.exists());
        assert!(fs::symlink_metadata(install_link).is_err());
        assert!(fs::symlink_metadata(short_bin).is_err());
        assert!(fs::symlink_metadata(extra_bin).is_err());
        assert!(fs::symlink_metadata(man_link).is_err());
        assert!(
            !fixture
                .roots
                .state_dir
                .join("repo-tool.repo.stamp")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_preserves_unowned_regular_command() {
        let fixture = Fixture::new("repo-regular-command");
        let install_root = fixture.roots.install_dir.join("owner/tool");
        let public = fixture.roots.bin_dir.join("tool");
        fs::create_dir_all(install_root.join("bin")).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        fs::write(install_root.join("bin/tool"), "managed command").unwrap();
        fs::write(&public, "generated client adapter").unwrap();
        let before = fs::metadata(&public).unwrap();

        let summary = remove_for_test(
            &ManifestEntry::new(
                "owner/tool",
                "github:repo",
                "tool",
                install_root.to_string_lossy(),
            ),
            &fixture.roots,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "generated client adapter"
        );
        let after = fs::metadata(&public).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        assert!(!install_root.exists());
        assert!(!summary.removed.contains(&public));
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/tool", Kind::Bin).exists(),
            "preserved regular commands must never acquire Shdeps ownership state"
        );
    }

    #[test]
    fn github_repo_cleanup_cannot_target_another_managed_dependency() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-cross-dependency-path");
        let owned = fixture.roots.install_dir.join("owner/a");
        let other = fixture.roots.install_dir.join("owner/b");
        let public = fixture.roots.bin_dir.join("a");
        fs::create_dir_all(&owned).unwrap();
        fs::create_dir_all(other.join("bin")).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        fs::write(owned.join("owned"), "remove\n").unwrap();
        fs::write(other.join("sentinel"), "preserve\n").unwrap();
        fs::write(other.join("bin/a"), "other command\n").unwrap();
        symlink(other.join("bin/a"), &public).unwrap();

        remove_for_test(
            &ManifestEntry::new("owner/a", "github:repo", "a", other.display().to_string()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!owned.exists());
        assert_eq!(
            fs::read_to_string(other.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert!(
            fs::symlink_metadata(public)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn github_repo_cleanup_uses_locked_root_after_install_alias_retarget() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("repo-locked-root-retarget");
        let physical_a = fixture.dir.join("physical-a");
        let physical_b = fixture.dir.join("physical-b");
        let logical = fixture.dir.join("install-link");
        let root_a = physical_a.join("owner/tool");
        let root_b = physical_b.join("owner/tool");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_a.join("owned"), "remove\n").unwrap();
        fs::write(root_b.join("sentinel"), "preserve\n").unwrap();
        symlink(&physical_a, &logical).unwrap();
        let roots = Roots {
            install_dir: logical.clone(),
            ..fixture.roots.clone()
        };
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");
        let locked_root = safe_repo_root(&entry, &roots).unwrap();
        assert_eq!(locked_root, root_a);
        let evidence = super::capture_evidence(&entry, &roots).unwrap();

        fs::remove_file(&logical).unwrap();
        symlink(&physical_b, &logical).unwrap();
        super::remove_builtin_with_evidence(&entry, &roots, Some(&locked_root), false, evidence)
            .unwrap();

        assert!(!root_a.exists());
        assert_eq!(
            fs::read_to_string(root_b.join("sentinel")).unwrap(),
            "preserve\n"
        );
        assert_eq!(fs::read_link(logical).unwrap(), physical_b);
    }

    #[test]
    #[cfg(unix)]
    fn binary_method_cleanup_removes_binary_install_root_empty_parents_and_stamps() {
        let fixture = Fixture::new("binary");
        let entry = ManifestEntry::new(
            "owner/binary-tool",
            "github:release",
            "binary-tool",
            fixture.roots.bin_dir.join("binary-tool").to_string_lossy(),
        );
        let helper = fixture.roots.bin_dir.join("binary-helper");
        let helper_target = fixture
            .roots
            .install_dir
            .join("owner/binary-tool/bin/binary-helper");
        fs::create_dir_all(helper_target.parent().unwrap()).unwrap();
        fs::write(&helper_target, "#!/bin/sh\n").unwrap();
        let tool_target = fixture
            .roots
            .install_dir
            .join("owner/binary-tool/bin/binary-tool");
        fs::write(&tool_target, "#!/bin/sh\n").unwrap();
        fs::create_dir_all(fixture.roots.bin_dir.clone()).unwrap();
        symlink(&tool_target, fixture.roots.bin_dir.join("binary-tool")).unwrap();
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        symlink(&helper_target, &helper).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/binary-tool", Kind::Bin),
            &[fixture.roots.bin_dir.join("binary-tool"), helper.clone()],
        )
        .unwrap();
        fixture.write_install("owner/binary-tool/artifact", "data\n");
        fixture.write_state("owner/binary-tool.release.stamp", "1\n");

        remove_for_test(&entry, &fixture.roots).unwrap();

        assert!(!fixture.roots.bin_dir.join("binary-tool").exists());
        assert!(!helper.exists());
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/binary-tool", Kind::Bin).exists()
        );
        assert!(!fixture.roots.install_dir.join("owner/binary-tool").exists());
        assert!(!fixture.roots.install_dir.join("owner").exists());
        assert!(
            !fixture
                .roots
                .state_dir
                .join("owner/binary-tool.release.stamp")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn binary_method_cleanup_preserves_tracked_command_retargeted_to_another_install() {
        let fixture = Fixture::new("binary-retargeted-command");
        let old_root = fixture.roots.install_dir.join("old-tool");
        let replacement_root = fixture.roots.install_dir.join("replacement-tool");
        let old_target = old_root.join("bin/tool");
        let replacement_target = replacement_root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");

        fs::create_dir_all(old_target.parent().unwrap()).unwrap();
        fs::write(&old_target, "old command\n").unwrap();
        fs::create_dir_all(replacement_target.parent().unwrap()).unwrap();
        fs::write(&replacement_target, "replacement command\n").unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&replacement_target, &public).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "old-tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();

        let summary = remove_for_test(
            &ManifestEntry::new("old-tool", "cargo", "tool", old_root.to_string_lossy()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!old_root.exists());
        assert_eq!(
            fs::read_link(&public).unwrap(),
            replacement_target,
            "stale path-only ownership must not remove another install's live command"
        );
        assert!(replacement_root.exists());
        assert!(!summary.removed.contains(&public));
        assert!(!link_state::path(&fixture.roots.state_dir, "old-tool", Kind::Bin).exists());
    }

    #[test]
    #[cfg(unix)]
    fn binary_method_cleanup_does_not_follow_retargeted_install_root() {
        let fixture = Fixture::new("binary-retargeted-root");
        let old_root = fixture.roots.install_dir.join("old-tool");
        let replacement_root = fixture.roots.install_dir.join("replacement-tool");
        let replacement_target = replacement_root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");

        fs::create_dir_all(replacement_target.parent().unwrap()).unwrap();
        fs::write(&replacement_target, "replacement command\n").unwrap();
        fs::create_dir_all(old_root.parent().unwrap()).unwrap();
        symlink(&replacement_root, &old_root).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&replacement_target, &public).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "old-tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();

        remove_for_test(
            &ManifestEntry::new("old-tool", "cargo", "tool", old_root.to_string_lossy()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!old_root.exists());
        assert_eq!(fs::read_link(&public).unwrap(), replacement_target);
        assert_eq!(
            fs::read_to_string(replacement_root.join("bin/tool")).unwrap(),
            "replacement command\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn binary_symlink_method_cleanup_preserves_regular_public_path() {
        let fixture = Fixture::new("binary-regular-public");
        let old_root = fixture.roots.install_dir.join("old-tool");
        let public = fixture.roots.bin_dir.join("tool");
        fixture.write_install("old-tool/bin/tool", "old command\n");
        fixture.write_bin("tool", "replacement command\n");

        remove_for_test(
            &ManifestEntry::new("old-tool", "cargo", "tool", old_root.to_string_lossy()),
            &fixture.roots,
        )
        .unwrap();

        assert!(!old_root.exists());
        assert_eq!(fs::read_to_string(public).unwrap(), "replacement command\n");
    }

    #[test]
    #[cfg(unix)]
    fn repo_cleanup_uses_immediate_public_link_target_for_ownership() {
        let fixture = Fixture::new("repo-nested-command-symlink");
        let repo_root = fixture.roots.install_dir.join("owner/repo-tool");
        let external = fixture.dir.join("external/tool");
        let nested = repo_root.join("bin/tool");
        let public = fixture.roots.bin_dir.join("tool");

        fs::create_dir_all(external.parent().unwrap()).unwrap();
        fs::write(&external, "external command\n").unwrap();
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        symlink(&external, &nested).unwrap();
        fs::create_dir_all(&fixture.roots.bin_dir).unwrap();
        symlink(&nested, &public).unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/repo-tool", Kind::Bin),
            std::slice::from_ref(&public),
        )
        .unwrap();

        remove_for_test(
            &ManifestEntry::new("owner/repo-tool", "github:repo", "tool", ""),
            &fixture.roots,
        )
        .unwrap();

        assert!(fs::symlink_metadata(public).is_err());
        assert_eq!(fs::read_to_string(external).unwrap(), "external command\n");
    }

    #[test]
    #[cfg(unix)]
    fn archive_cleanup_preserves_regular_public_launcher() {
        let fixture = Fixture::new("archive-launcher");
        let entry = ManifestEntry::new(
            "owner/archive-tool",
            "github:release",
            "archive-tool",
            fixture.roots.bin_dir.join("archive-tool").to_string_lossy(),
        );
        let public = fixture.roots.bin_dir.join("archive-tool");
        let helper = fixture.roots.bin_dir.join("archive-helper");
        let helper_target = fixture
            .roots
            .install_dir
            .join("owner/archive-tool/bin/archive-helper");
        fs::create_dir_all(helper_target.parent().unwrap()).unwrap();
        fs::write(&helper_target, "#!/bin/sh\n").unwrap();
        fixture.write_bin("archive-tool", "#!/bin/sh\n# user launcher\n");
        symlink(&helper_target, &helper).unwrap();
        fs::write(
            github_release_install::archive_layout_path(
                &fixture.roots.install_dir,
                "owner/archive-tool",
            ),
            "v1 archive\n",
        )
        .unwrap();
        link_state::write(
            &link_state::path(&fixture.roots.state_dir, "owner/archive-tool", Kind::Bin),
            std::slice::from_ref(&helper),
        )
        .unwrap();

        remove_for_test(&entry, &fixture.roots).unwrap();

        assert_eq!(
            fs::read_to_string(&public).unwrap(),
            "#!/bin/sh\n# user launcher\n"
        );
        assert!(!helper.exists());
        assert!(
            !link_state::path(&fixture.roots.state_dir, "owner/archive-tool", Kind::Bin).exists()
        );
        assert!(
            !fixture
                .roots
                .install_dir
                .join("owner/archive-tool")
                .exists()
        );
    }

    #[test]
    fn package_cleanup_preserves_system_owned_artifacts() {
        let fixture = Fixture::new("pkg");
        fixture.write_bin("pkg-tool", "#!/bin/sh\n");
        crate::package_proof::write(
            &fixture.roots.state_dir,
            "pkg-tool",
            "apt",
            "pkg-tool",
            "pkg-tool",
        )
        .unwrap();

        let summary = remove_for_test(
            &ManifestEntry::new("pkg-tool", "pkg", "pkg-tool", ""),
            &fixture.roots,
        )
        .unwrap();

        assert!(summary.preserved_package);
        assert!(fixture.roots.bin_dir.join("pkg-tool").exists());
        assert!(!crate::package_proof::path(&fixture.roots.state_dir, "pkg-tool").exists());
    }

    #[test]
    fn custom_cleanup_only_removes_shdeps_stamps() {
        let fixture = Fixture::new("custom");
        fixture.write_state("custom-tool.custom.stamp", "1\n");

        let summary = remove_for_test(
            &ManifestEntry::new("custom-tool", "custom", "custom-tool", ""),
            &fixture.roots,
        )
        .unwrap();

        assert!(summary.custom_requires_hook);
        assert!(
            !fixture
                .roots
                .state_dir
                .join("custom-tool.custom.stamp")
                .exists()
        );
    }

    #[test]
    fn remove_stamps_does_not_delete_stamps_for_dep_with_matching_name_prefix() {
        // Two deps in the same namespace directory — `owner/tool` and
        // `owner/tool.extra` — share the `tool.` filename prefix. Pruning one
        // must not delete the other's stamps.
        let fixture = Fixture::new("stamp-prefix");
        fixture.write_state("owner/tool.repo.stamp", "1\n");
        fixture.write_state("owner/tool.extra.repo.stamp", "1\n");

        let mut summary = Summary::default();
        remove_stamps(&fixture.roots.state_dir, "owner/tool", &mut summary).unwrap();

        assert!(
            !fixture
                .roots
                .state_dir
                .join("owner/tool.repo.stamp")
                .exists(),
            "tool's own stamp should be removed"
        );
        assert!(
            fixture
                .roots
                .state_dir
                .join("owner/tool.extra.repo.stamp")
                .exists(),
            "tool.extra's stamp must not be removed by pruning tool"
        );
    }

    #[test]
    fn safe_repo_root_uses_named_fallback_when_recorded_path_is_empty() {
        let dir = crate::test_support::temp_dir("shdeps-cleanup-empty-repo-path");
        let roots = Roots {
            state_dir: dir.join("state"),
            install_dir: dir.join("home"),
            bin_dir: dir.join("bin"),
        };
        let entry = ManifestEntry::new("owner/tool", "github:repo", "tool", "");

        assert_eq!(
            safe_repo_root(&entry, &roots),
            Some(roots.install_dir.join("owner/tool"))
        );
    }

    #[test]
    fn safe_repo_root_canonicalizes_legacy_git_suffix() {
        let fixture = Fixture::new("repo-root-git-suffix");
        let entry = ManifestEntry::new("owner/tool.git", "github:repo", "tool", "");

        assert_eq!(
            safe_repo_root(&entry, &fixture.roots),
            Some(fixture.roots.install_dir.join("owner/tool"))
        );
    }

    #[test]
    fn github_repo_cleanup_ignores_tampered_recorded_install_path() {
        // Repository cleanup derives ownership from the validated name. A
        // recorded absolute path is diagnostic only and cannot redirect it.
        let fixture = Fixture::new("tampered-install-path");
        let bystander = fixture.dir.join("bystander");
        fs::create_dir_all(&bystander).unwrap();
        fs::write(bystander.join("data"), "user-owned").unwrap();
        let absolute = bystander.to_string_lossy().into_owned();

        remove_for_test(
            &ManifestEntry::new("repo-tool", "github:repo", "repo-tool", &absolute),
            &fixture.roots,
        )
        .unwrap();

        assert!(bystander.exists(), "external path must be preserved");
        assert!(bystander.join("data").exists());
    }

    fn entry(name: &str, method: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            method: method.to_owned(),
            cmd: name.to_owned(),
            cmd_explicit: false,
            aliases: String::new(),
            filter: String::new(),
        }
    }

    struct Fixture {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = crate::test_support::temp_dir(&format!("shdeps-cleanup-{name}"));
            let roots = Roots {
                state_dir: dir.join("state"),
                install_dir: dir.join("share"),
                bin_dir: dir.join("bin"),
            };
            Self { dir, roots }
        }

        fn write_bin(&self, name: &str, content: &str) {
            self.write_at(&self.roots.bin_dir.join(name), content);
        }

        fn write_install(&self, rel: impl AsRef<Path>, content: &str) {
            self.write_at(&self.roots.install_dir.join(rel), content);
        }

        fn write_state(&self, rel: impl AsRef<Path>, content: &str) {
            self.write_at(&self.roots.state_dir.join(rel), content);
        }

        fn write_at(&self, path: &Path, content: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
}
