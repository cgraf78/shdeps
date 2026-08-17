//! Recoverable publication of one `github:repo` checkout root.
//!
//! A non-empty directory cannot be replaced atomically with a symbolic link on
//! every supported Unix filesystem. Shdeps therefore owns one deliberately
//! narrow sibling journal for repository-root type changes. The shared
//! checkout lock excludes another well-behaved writer while a run is alive;
//! the journal exists only so the next run can finish or roll back after an
//! uncatchable process death.
//!
//! The generated checkout installer owns a different transaction format. We
//! do not duplicate its parser here: finding that transaction while holding
//! the shared lock means its owner died, so Shdeps fails closed and asks the
//! operator to rerun the installer that knows how to recover it.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Result;

const FORMAT: &str = "shdeps repository transition v1";
const RECORD: &str = "record";
const PREVIOUS: &str = "previous";
const BLOCKED: &str = "blocked";
const BLOCKED_CONTENT: &str = "shdeps repository transition blocked v1\n";
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// No-follow identity for an object whose ownership the transaction records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    kind: IdentityKind,
    device: u64,
    inode: u64,
    target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum IdentityKind {
    Directory,
    Symlink,
}

impl Identity {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_dir() {
            return Ok(Self {
                kind: IdentityKind::Directory,
                device: metadata.dev(),
                inode: metadata.ino(),
                target: None,
            });
        }
        if metadata.file_type().is_symlink() {
            return Ok(Self {
                kind: IdentityKind::Symlink,
                device: metadata.dev(),
                inode: metadata.ino(),
                target: Some(fs::read_link(path)?),
            });
        }
        Err(invalid_transition(format!(
            "repository root is neither a real directory nor a symlink: {}",
            path.display()
        )))
    }

    fn validate(&self) -> Result<()> {
        match (self.kind, self.target.as_ref()) {
            (IdentityKind::Directory, None) | (IdentityKind::Symlink, Some(_)) => Ok(()),
            _ => Err(invalid_transition(
                "repository transition identity has an inconsistent target",
            )),
        }
    }

    fn is_directory(&self) -> bool {
        self.kind == IdentityKind::Directory
    }

    fn matches(&self, path: &Path) -> bool {
        Self::read(path).is_ok_and(|actual| actual == *self)
    }
}

/// Exact object which a publication is allowed to leave at the stable path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Desired {
    Directory { identity: Identity, staged: PathBuf },
    Symlink(PathBuf),
}

impl Desired {
    fn validate(&self, checkout: &Path) -> Result<()> {
        match self {
            Self::Directory { identity, staged } if identity.is_directory() => {
                let checkout_parent = checkout.parent();
                let checkout_name = checkout.file_name().and_then(|name| name.to_str());
                let staged_name = staged.file_name().and_then(|name| name.to_str());
                let valid_staged = match (checkout_parent, checkout_name, staged_name) {
                    (Some(parent), Some(name), Some(staged_name))
                        if staged.parent() == Some(parent) =>
                    {
                        staged_name
                            .strip_prefix(&format!("{name}.tmp."))
                            .is_some_and(|pid| {
                                !pid.is_empty()
                                    && pid.bytes().all(|byte| byte.is_ascii_digit())
                                    && pid.parse::<u32>().is_ok_and(|pid| pid > 0)
                            })
                    }
                    _ => false,
                };
                if !valid_staged {
                    return Err(invalid_transition(
                        "repository transition staged directory is not the reserved sibling clone path",
                    ));
                }
                identity.validate()
            }
            Self::Directory { .. } => Err(invalid_transition(
                "repository transition destination is not a directory identity",
            )),
            Self::Symlink(target) if target.as_os_str().is_empty() => Err(invalid_transition(
                "repository transition symlink target is empty",
            )),
            Self::Symlink(_) => Ok(()),
        }
    }

    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Directory { identity, .. } => identity.matches(path),
            Self::Symlink(target) => fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.file_type().is_symlink()
                    && fs::read_link(path).is_ok_and(|actual| actual == *target)
            }),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: String,
    checkout: PathBuf,
    previous: Identity,
    desired: Desired,
}

impl Record {
    fn validate(&self, checkout: &Path) -> Result<()> {
        if self.format != FORMAT || self.checkout != checkout {
            return Err(invalid_transition(format!(
                "repository transition record does not belong to {}",
                checkout.display()
            )));
        }
        self.previous.validate()?;
        self.desired.validate(checkout)
    }
}

/// Recovers any Shdeps-owned transition and rejects installer-owned state.
pub(crate) fn recover(checkout: &Path) -> Result<()> {
    let actions = actions_transaction_path(checkout)?;
    if path_present(&actions)? {
        return Err(invalid_transition(format!(
            "checkout installer transaction is still present at {}; rerun the checkout installer before Shdeps",
            actions.display()
        )));
    }
    recover_shdeps(checkout, true)
}

/// Publishes a validated development checkout at the canonical repository root.
pub(crate) fn publish_development(
    checkout: &Path,
    target: &Path,
    replace_owned_destination: bool,
) -> Result<()> {
    recover(checkout)?;
    match fs::symlink_metadata(checkout) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            symlink(target, checkout)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(checkout).is_ok_and(|current| current == target) =>
        {
            Ok(())
        }
        Ok(_) if !replace_owned_destination => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository destination appeared before development-link publication",
        )
        .into()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            replace_owned_symlink(checkout, target)
        }
        Ok(metadata) if metadata.file_type().is_dir() => {
            let previous = Identity::read(checkout)?;
            publish_transaction(
                checkout,
                previous,
                Desired::Symlink(target.to_path_buf()),
                || symlink(target, checkout).map_err(Into::into),
            )
        }
        Ok(_) => Err(invalid_transition(format!(
            "refusing to replace non-repository object at {}",
            checkout.display()
        ))),
    }
}

/// Publishes a staged managed directory, recovering an old directory or link.
pub(crate) fn publish_directory(
    checkout: &Path,
    staged: &Path,
    replace_owned_destination: bool,
) -> Result<()> {
    recover(checkout)?;
    let desired_identity = Identity::read(staged)?;
    if !desired_identity.is_directory() {
        return Err(invalid_transition(format!(
            "staged repository is not a real directory: {}",
            staged.display()
        )));
    }

    match fs::symlink_metadata(checkout) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            rename_noreplace(staged, checkout)?;
            if desired_identity.matches(checkout) {
                Ok(())
            } else {
                Err(invalid_transition(
                    "published repository directory identity changed",
                ))
            }
        }
        Err(error) => Err(error.into()),
        Ok(_) if !replace_owned_destination => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository destination appeared before managed-checkout publication",
        )
        .into()),
        Ok(metadata) if metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            let previous = Identity::read(checkout)?;
            publish_transaction(
                checkout,
                previous,
                Desired::Directory {
                    identity: desired_identity,
                    staged: staged.to_path_buf(),
                },
                || rename_noreplace(staged, checkout).map_err(Into::into),
            )
        }
        Ok(_) => Err(invalid_transition(format!(
            "refusing to replace non-repository object at {}",
            checkout.display()
        ))),
    }
}

// Ordering is the crash-recovery contract: persist recovery authority before
// vacating the stable path, park the exact previous generation, publish the
// desired object, then use the same classifier for commit or rollback. Moving
// any live mutation before `begin` could strand an unrecorded generation after
// SIGKILL.
fn publish_transaction(
    checkout: &Path,
    previous: Identity,
    desired: Desired,
    publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let journal = begin(checkout, previous, desired.clone())?;
    let previous_path = journal.join(PREVIOUS);
    if let Err(error) = fs::rename(checkout, &previous_path) {
        let _ = recover_shdeps(checkout, false);
        return Err(error.into());
    }

    if let Err(error) = publish() {
        return match recover_shdeps(checkout, false) {
            Ok(()) => Err(error),
            Err(recovery) => Err(invalid_transition(format!(
                "repository publication failed ({error}); recovery also failed ({recovery})"
            ))),
        };
    }
    if !desired.matches(checkout) {
        if is_real_directory(checkout) {
            mark_blocked(&journal)?;
        }
        return Err(invalid_transition(
            "published repository does not match its transition record",
        ));
    }
    recover_shdeps(checkout, false)
}

fn begin(checkout: &Path, previous: Identity, desired: Desired) -> Result<PathBuf> {
    let actions = actions_transaction_path(checkout)?;
    if path_present(&actions)? {
        return Err(invalid_transition(format!(
            "checkout installer transaction is still present at {}; rerun the checkout installer before Shdeps",
            actions.display()
        )));
    }
    if !previous.matches(checkout) {
        return Err(invalid_transition(
            "repository root changed before transition preparation",
        ));
    }
    previous.validate()?;
    desired.validate(checkout)?;

    let journal = journal_path(checkout)?;
    if path_present(&journal)? {
        return Err(invalid_transition(format!(
            "repository transition already exists: {}",
            journal.display()
        )));
    }
    fs::DirBuilder::new().mode(0o700).create(&journal)?;
    let record = Record {
        format: FORMAT.to_owned(),
        checkout: checkout.to_path_buf(),
        previous,
        desired,
    };
    let mut encoded = serde_json::to_string_pretty(&record)?;
    encoded.push('\n');
    if let Err(error) = crate::state::write_atomic(&journal.join(RECORD), &encoded) {
        let _ = fs::remove_dir(&journal);
        return Err(error);
    }
    Ok(journal)
}

/// Recovers the private journal with authority appropriate to this observation.
///
/// `allow_later_writer` is true at public, pre-transaction recovery entrypoints,
/// where a real directory may be a checkout-installer generation that filled
/// Shdeps' crash gap. Internal recovery of a transition started by the current
/// call passes false: a collision first observed during that transition is not
/// retroactively granted co-owner authority and must be marked blocked instead.
fn recover_shdeps(checkout: &Path, allow_later_writer: bool) -> Result<()> {
    let journal = journal_path(checkout)?;
    let metadata = match fs::symlink_metadata(&journal) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) => metadata,
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_transition(format!(
            "repository transition is not a private directory: {}",
            journal.display()
        )));
    }

    let entries = journal_entries(&journal)?;
    if entries.is_empty() {
        // Cleanup removes the record before the final rmdir. An uncatchable
        // death in that tiny window is safe to finish because no backup object
        // remains and the stable checkout was already classified beforehand.
        fs::remove_dir(&journal)?;
        return Ok(());
    }
    if !entries.iter().any(|entry| entry == RECORD)
        && !entries.iter().any(|entry| entry == PREVIOUS)
        && path_present(checkout)?
        && entries
            .iter()
            .all(|entry| entry.starts_with(".record.tmp."))
    {
        // `state::write_atomic` creates its temp inside the new private
        // journal. SIGKILL before the record rename cannot have moved the
        // checkout yet, so this exact shape is inert preparation debris. Do
        // not broaden the match: any symlink, hardlink, unexpected basename,
        // missing checkout, or published backup remains a fail-closed state.
        for entry in &entries {
            let path = journal.join(entry);
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.nlink() != 1
            {
                return Err(invalid_transition(
                    "repository transition preparation debris is not a private file",
                ));
            }
        }
        for entry in entries {
            fs::remove_file(journal.join(entry))?;
        }
        fs::remove_dir(&journal)?;
        return Ok(());
    }
    let blocked_temps = entries
        .iter()
        .filter(|entry| entry.starts_with(".blocked.tmp."))
        .cloned()
        .collect::<Vec<_>>();
    for entry in &blocked_temps {
        let metadata = fs::symlink_metadata(journal.join(entry))?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.nlink() != 1
        {
            return Err(invalid_transition(
                "repository transition collision preparation is not a private file",
            ));
        }
    }
    let canonical_blocked = entries.iter().any(|entry| entry == BLOCKED);
    let blocked = canonical_blocked || !blocked_temps.is_empty();
    if canonical_blocked {
        let blocked_path = journal.join(BLOCKED);
        let bytes = crate::state::read_private_bounded(&blocked_path, 1024)?;
        if bytes != BLOCKED_CONTENT.as_bytes() {
            return Err(invalid_transition(
                "repository transition collision marker is malformed",
            ));
        }
    }
    if entries.iter().any(|entry| {
        entry != RECORD
            && entry != PREVIOUS
            && entry != BLOCKED
            && !entry.starts_with(".blocked.tmp.")
    }) {
        return Err(invalid_transition(format!(
            "repository transition contains unexpected state: {}",
            journal.display()
        )));
    }

    let record = read_record(&journal.join(RECORD), checkout)?;
    let previous_path = journal.join(PREVIOUS);
    let previous_present = path_present(&previous_path)?;
    let checkout_present = path_present(checkout)?;

    if previous_present {
        if !record.previous.matches(&previous_path) {
            return Err(invalid_transition(
                "repository transition backup identity does not match its record",
            ));
        }
        if !checkout_present {
            return restore_parked_previous(
                &journal,
                checkout,
                &record,
                blocked,
                allow_later_writer,
            );
        }
        return finish_with_live_checkout(&journal, checkout, &record, blocked, allow_later_writer);
    }

    if !checkout_present {
        return Err(invalid_transition(
            "repository transition lost both stable and backup objects",
        ));
    }
    if record.previous.matches(checkout)
        || record.desired.matches(checkout)
        || (!blocked && allow_later_writer && is_real_directory(checkout))
    {
        // Preparation stopped before the move, cleanup had already removed the
        // backup, or a co-owner published a real checkout generation.
        return remove_journal(&journal, &record);
    }
    if !allow_later_writer && is_real_directory(checkout) {
        mark_blocked(&journal)?;
    }
    Err(invalid_transition(
        "repository transition stable object does not match its record",
    ))
}

/// Restores a parked generation, reclassifying an atomic no-replace loss.
///
/// The stable path was absent at the caller's snapshot, but a non-cooperating
/// writer can still publish before the exclusive rename. Treating that error
/// as a generic rollback failure would let the next run mistake an immediate
/// collision for a later checkout-installer generation. Reusing the live-root
/// classifier here makes that distinction durable through the `blocked`
/// marker while preserving the valid co-owner handoff on ordinary recovery.
fn restore_parked_previous(
    journal: &Path,
    checkout: &Path,
    record: &Record,
    blocked: bool,
    _outer_allows_later_writer: bool,
) -> Result<()> {
    let previous_path = journal.join(PREVIOUS);
    match rename_noreplace(&previous_path, checkout) {
        Ok(()) => {
            if !record.previous.matches(checkout) {
                return Err(invalid_transition(
                    "repository transition rollback identity changed",
                ));
            }
            remove_journal(journal, record)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // This writer appeared during the current lock-held rollback, not
            // before we acquired the lock. It therefore cannot inherit the
            // outer call's permission to accept a checkout-installer winner.
            // Exact desired state still commits through the first classifier
            // branch, while any new real directory is durably blocked.
            finish_with_live_checkout(journal, checkout, record, blocked, false)
        }
        Err(error) => Err(error.into()),
    }
}

fn finish_with_live_checkout(
    journal: &Path,
    checkout: &Path,
    record: &Record,
    blocked: bool,
    allow_later_writer: bool,
) -> Result<()> {
    let previous_path = journal.join(PREVIOUS);
    if record.desired.matches(checkout)
        || (!blocked && allow_later_writer && is_real_directory(checkout))
    {
        // Either our desired publication committed, or the checkout installer
        // acquired the shared lock after our death and filled the absent stable
        // path with a new managed generation. In both cases the stable root
        // wins and only our exact backup is retired.
        remove_identity(&previous_path, &record.previous)?;
        return remove_journal(journal, record);
    }
    if !allow_later_writer && is_real_directory(checkout) {
        mark_blocked(journal)?;
    }
    let detail = if blocked {
        "repository transition previously observed a foreign live collision; move the foreign stable object aside and rerun so the exact backup can be restored"
    } else {
        "repository transition found a foreign stable checkout object"
    };
    Err(invalid_transition(detail))
}

fn mark_blocked(journal: &Path) -> Result<()> {
    crate::state::write_atomic(&journal.join(BLOCKED), BLOCKED_CONTENT)
}

fn read_record(path: &Path, checkout: &Path) -> Result<Record> {
    let bytes = crate::state::read_private_bounded(path, MAX_RECORD_BYTES).map_err(|error| {
        invalid_transition(format!(
            "cannot read repository transition record {}: {error}",
            path.display()
        ))
    })?;
    if bytes.contains(&0) {
        return Err(invalid_transition(
            "repository transition record contains a NUL byte",
        ));
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(|error| {
        invalid_transition(format!("malformed repository transition record: {error}"))
    })?;
    record.validate(checkout)?;
    Ok(record)
}

fn journal_entries(path: &Path) -> Result<Vec<String>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(invalid_transition(
                "repository transition contains a non-UTF-8 entry",
            ));
        };
        entries.push(name);
    }
    entries.sort();
    Ok(entries)
}

fn remove_identity(path: &Path, identity: &Identity) -> Result<()> {
    if !identity.matches(path) {
        return Err(invalid_transition(
            "repository transition object changed before cleanup",
        ));
    }
    match identity.kind {
        IdentityKind::Directory => fs::remove_dir_all(path)?,
        IdentityKind::Symlink => fs::remove_file(path)?,
    }
    Ok(())
}

fn remove_journal(journal: &Path, record: &Record) -> Result<()> {
    if let Desired::Directory { identity, staged } = &record.desired {
        match fs::symlink_metadata(staged) {
            Ok(_) if identity.matches(staged) => remove_identity(staged, identity)?,
            Ok(_) => {
                return Err(invalid_transition(format!(
                    "staged repository changed before transition cleanup: {}",
                    staged.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match fs::remove_file(journal.join(BLOCKED)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for entry in journal_entries(journal)? {
        if entry.starts_with(".blocked.tmp.") {
            let path = journal.join(entry);
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.nlink() != 1
            {
                return Err(invalid_transition(
                    "repository transition collision preparation changed during cleanup",
                ));
            }
            fs::remove_file(path)?;
        }
    }
    let record = journal.join(RECORD);
    match fs::remove_file(&record) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::remove_dir(journal)?;
    Ok(())
}

fn replace_owned_symlink(checkout: &Path, target: &Path) -> Result<()> {
    // This is the sole intentional replace-style publication: the shared lock
    // is held and recorded Shdeps state authorizes destination replacement.
    // Symlink-to-symlink rename is one atomic commit and needs no vacancy
    // journal. Absent, unrecorded, and directory roots instead require
    // no-replace publication or the recoverable transaction above.
    let parent = checkout.parent().ok_or_else(|| {
        invalid_transition("repository checkout has no parent for symlink publication")
    })?;
    let name = checkout
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_transition("repository checkout has no UTF-8 basename"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let staged = parent.join(format!(
        ".{name}.shdeps-development-link.{}.{stamp}",
        std::process::id()
    ));
    symlink(target, &staged)?;
    let result = fs::rename(&staged, checkout);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result.map_err(Into::into)
}

/// Atomically publishes `source` only when `destination` is still absent.
///
/// Plain POSIX `rename` may replace a late empty directory, which would defeat
/// the unrecorded-root no-clobber guarantee and can also discard a co-owner
/// generation during transaction rollback. The supported fleet platforms all
/// expose an atomic exclusive rename; unknown Unix targets fail closed rather
/// than silently falling back to clobbering semantics.
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source path contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains a NUL byte",
        )
    })?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let status = unsafe {
        // SAFETY: both C strings are live and NUL-terminated for the syscall;
        // AT_FDCWD makes both absolute/path-relative arguments use cwd exactly
        // like `rename`, and the only flag is the kernel no-replace contract.
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };

    #[cfg(target_os = "macos")]
    let status = unsafe {
        // SAFETY: the paths are valid C strings and RENAME_EXCL is the macOS
        // atomic no-replace equivalent of Linux RENAME_NOREPLACE.
        libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL)
    };

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace repository publication is unsupported on this Unix platform",
        ))
    }
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn journal_path(checkout: &Path) -> Result<PathBuf> {
    sibling_path(checkout, "shdeps-repo-transition-v1")
}

fn actions_transaction_path(checkout: &Path) -> Result<PathBuf> {
    sibling_path(checkout, "install.transaction")
}

fn sibling_path(checkout: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = checkout
        .parent()
        .ok_or_else(|| invalid_transition("repository checkout has no parent directory"))?;
    let name = checkout
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_transition("repository checkout has no UTF-8 basename"))?;
    Ok(parent.join(format!(".{name}.{suffix}")))
}

fn invalid_transition(message: impl Into<String>) -> crate::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()).into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use super::{
        Desired, Identity, actions_transaction_path, begin, journal_path, publish_development,
        publish_directory, publish_transaction, recover, rename_noreplace, restore_parked_previous,
    };

    #[test]
    fn directory_to_development_link_is_published_without_debris() {
        let dir = temp_dir("directory-to-link");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");

        publish_development(&checkout, &development, true).unwrap();

        assert_eq!(fs::read_link(&checkout).unwrap(), development);
        assert!(!journal_path(&checkout).unwrap().exists());
    }

    #[test]
    fn interrupted_directory_to_link_rolls_back_an_absent_checkout() {
        let dir = temp_dir("directory-link-rollback");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();
        let journal = begin(&checkout, previous, Desired::Symlink(development.clone())).unwrap();
        fs::rename(&checkout, journal.join("previous")).unwrap();

        recover(&checkout).unwrap();

        assert_eq!(fs::read_to_string(checkout.join("old")).unwrap(), "managed");
        assert!(!journal.exists());
    }

    #[test]
    fn interrupted_directory_to_link_finishes_an_exact_publication() {
        let dir = temp_dir("directory-link-commit");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();
        let journal = begin(&checkout, previous, Desired::Symlink(development.clone())).unwrap();
        fs::rename(&checkout, journal.join("previous")).unwrap();
        symlink(&development, &checkout).unwrap();

        recover(&checkout).unwrap();

        assert_eq!(fs::read_link(&checkout).unwrap(), development);
        assert!(!journal.exists());
    }

    #[test]
    fn installer_directory_wins_an_interrupted_link_publication() {
        let dir = temp_dir("installer-wins");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();
        let journal = begin(&checkout, previous, Desired::Symlink(development)).unwrap();
        fs::rename(&checkout, journal.join("previous")).unwrap();
        write_file(&checkout.join("installer"), "new generation");

        recover(&checkout).unwrap();

        assert_eq!(
            fs::read_to_string(checkout.join("installer")).unwrap(),
            "new generation"
        );
        assert!(!journal.exists());
    }

    #[test]
    fn immediate_publication_race_preserves_foreign_directory_and_backup() {
        let dir = temp_dir("immediate-publication-race");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();

        let error = publish_transaction(&checkout, previous, Desired::Symlink(development), || {
            write_file(&checkout.join("foreign"), "preserve");
            Err(std::io::Error::other("publication collision").into())
        })
        .unwrap_err();

        let journal = journal_path(&checkout).unwrap();
        assert!(error.to_string().contains("recovery also failed"));
        assert_eq!(
            fs::read_to_string(checkout.join("foreign")).unwrap(),
            "preserve"
        );
        assert_eq!(
            fs::read_to_string(journal.join("previous/old")).unwrap(),
            "managed"
        );
        assert!(journal.join("record").is_file());
        assert!(journal.join("blocked").is_file());
        fs::rename(
            journal.join("blocked"),
            journal.join(".blocked.tmp.123.456.abcdef"),
        )
        .unwrap();

        let recovery = recover(&checkout).unwrap_err();
        assert!(recovery.to_string().contains("foreign live collision"));
        assert_eq!(
            fs::read_to_string(checkout.join("foreign")).unwrap(),
            "preserve"
        );
        assert_eq!(
            fs::read_to_string(journal.join("previous/old")).unwrap(),
            "managed"
        );

        let foreign = checkout.with_extension("foreign");
        fs::rename(&checkout, &foreign).unwrap();
        recover(&checkout).unwrap();
        assert_eq!(fs::read_to_string(checkout.join("old")).unwrap(), "managed");
        assert_eq!(
            fs::read_to_string(foreign.join("foreign")).unwrap(),
            "preserve"
        );
        assert!(!journal.exists());
    }

    #[test]
    fn mismatched_successful_publication_is_durably_fail_closed() {
        let dir = temp_dir("mismatched-success");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();

        let error = publish_transaction(&checkout, previous, Desired::Symlink(development), || {
            write_file(&checkout.join("foreign"), "preserve");
            Ok(())
        })
        .unwrap_err();

        let journal = journal_path(&checkout).unwrap();
        assert!(error.to_string().contains("does not match"));
        assert!(journal.join("blocked").is_file());
        assert!(recover(&checkout).is_err());
        assert_eq!(
            fs::read_to_string(checkout.join("foreign")).unwrap(),
            "preserve"
        );
        assert_eq!(
            fs::read_to_string(journal.join("previous/old")).unwrap(),
            "managed"
        );
    }

    #[test]
    fn interrupted_development_link_to_directory_rolls_back() {
        let dir = temp_dir("link-directory-rollback");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        let staged = dir.join("share/owner/tool.tmp.123");
        write_file(&development.join("bin/tool"), "development");
        write_file(&staged.join("managed"), "candidate");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        symlink(&development, &checkout).unwrap();
        let previous = Identity::read(&checkout).unwrap();
        let next = Identity::read(&staged).unwrap();
        let journal = begin(
            &checkout,
            previous,
            Desired::Directory {
                identity: next,
                staged: staged.clone(),
            },
        )
        .unwrap();
        fs::rename(&checkout, journal.join("previous")).unwrap();

        recover(&checkout).unwrap();

        assert_eq!(fs::read_link(&checkout).unwrap(), development);
        assert!(!staged.exists());
        assert!(!journal.exists());
    }

    #[test]
    fn development_link_to_directory_publication_is_recoverable() {
        let dir = temp_dir("link-to-directory");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        let staged = dir.join("share/owner/tool.tmp.123");
        write_file(&development.join("bin/tool"), "development");
        write_file(&staged.join("managed"), "candidate");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        symlink(&development, &checkout).unwrap();

        publish_directory(&checkout, &staged, true).unwrap();

        assert_eq!(
            fs::read_to_string(checkout.join("managed")).unwrap(),
            "candidate"
        );
        assert!(!staged.exists());
        assert!(!journal_path(&checkout).unwrap().exists());
    }

    #[test]
    fn unowned_directory_publication_preserves_late_destination() {
        let dir = temp_dir("unowned-directory-late-destination");
        let checkout = dir.join("share/owner/tool");
        let staged = dir.join("share/owner/tool.tmp.123");
        write_file(&staged.join("managed"), "candidate");
        fs::create_dir_all(&checkout).unwrap();

        let error = publish_directory(&checkout, &staged, false).unwrap_err();

        assert!(error.to_string().contains("destination appeared"));
        assert_eq!(fs::read_dir(&checkout).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(staged.join("managed")).unwrap(),
            "candidate"
        );
        assert!(!journal_path(&checkout).unwrap().exists());
    }

    #[test]
    fn exclusive_directory_rename_preserves_empty_destination() {
        let dir = temp_dir("exclusive-rename");
        let source = dir.join("source");
        let destination = dir.join("destination");
        write_file(&source.join("candidate"), "preserve");
        fs::create_dir_all(&destination).unwrap();

        let error = rename_noreplace(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(source.join("candidate")).unwrap(),
            "preserve"
        );
    }

    #[test]
    fn rollback_collision_is_blocked_before_later_recovery() {
        let dir = temp_dir("rollback-collision");
        let checkout = dir.join("share/owner/tool");
        let development = dir.join("git/tool");
        write_file(&checkout.join("old"), "managed");
        write_file(&development.join("bin/tool"), "development");
        let previous = Identity::read(&checkout).unwrap();
        let journal = begin(&checkout, previous, Desired::Symlink(development)).unwrap();
        fs::rename(&checkout, journal.join("previous")).unwrap();
        fs::create_dir_all(&checkout).unwrap();

        let record = super::read_record(&journal.join("record"), &checkout).unwrap();
        let error = restore_parked_previous(&journal, &checkout, &record, false, true).unwrap_err();

        assert!(error.to_string().contains("foreign stable"));
        assert!(journal.join("blocked").is_file());
        assert_eq!(fs::read_dir(&checkout).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(journal.join("previous/old")).unwrap(),
            "managed"
        );
        assert!(recover(&checkout).is_err());
    }

    #[test]
    fn actions_transaction_blocks_shdeps_without_mutation() {
        let dir = temp_dir("actions-transaction");
        let checkout = dir.join("share/owner/tool");
        write_file(&checkout.join("managed"), "preserve");
        let actions = actions_transaction_path(&checkout).unwrap();
        fs::create_dir_all(&actions).unwrap();

        let error = recover(&checkout).unwrap_err();

        assert!(error.to_string().contains("rerun the checkout installer"));
        assert_eq!(
            fs::read_to_string(checkout.join("managed")).unwrap(),
            "preserve"
        );
        assert!(actions.is_dir());
    }

    #[test]
    fn malformed_journal_fails_closed() {
        let dir = temp_dir("malformed");
        let checkout = dir.join("share/owner/tool");
        write_file(&checkout.join("managed"), "preserve");
        let journal = journal_path(&checkout).unwrap();
        fs::create_dir_all(&journal).unwrap();
        fs::write(journal.join("record"), "not a transaction\n").unwrap();

        let error = recover(&checkout).unwrap_err();

        assert!(error.to_string().contains("repository transition"));
        assert_eq!(
            fs::read_to_string(checkout.join("managed")).unwrap(),
            "preserve"
        );
        assert!(journal.is_dir());
    }

    #[test]
    fn tampered_staged_path_cannot_authorize_stable_checkout_deletion() {
        let dir = temp_dir("tampered-staged-path");
        let checkout = dir.join("share/owner/tool");
        write_file(&checkout.join("managed"), "preserve");
        let metadata = fs::symlink_metadata(&checkout).unwrap();
        let journal = journal_path(&checkout).unwrap();
        fs::create_dir_all(&journal).unwrap();
        let record = serde_json::json!({
            "format": "shdeps repository transition v1",
            "checkout": checkout.clone(),
            "previous": {
                "kind": "directory",
                "device": metadata.dev(),
                "inode": metadata.ino(),
                "target": null
            },
            "desired": {
                "Directory": {
                    "identity": {
                        "kind": "directory",
                        "device": metadata.dev(),
                        "inode": metadata.ino(),
                        "target": null
                    },
                    "staged": checkout.clone()
                }
            }
        });
        fs::write(
            journal.join("record"),
            format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
        )
        .unwrap();

        let error = recover(&checkout).unwrap_err();

        assert!(error.to_string().contains("reserved sibling clone path"));
        assert_eq!(
            fs::read_to_string(checkout.join("managed")).unwrap(),
            "preserve"
        );
        assert!(journal.is_dir());
    }

    #[test]
    fn interrupted_record_write_is_inert_and_recoverable() {
        let dir = temp_dir("record-write");
        let checkout = dir.join("share/owner/tool");
        write_file(&checkout.join("managed"), "preserve");
        let journal = journal_path(&checkout).unwrap();
        fs::create_dir_all(&journal).unwrap();
        fs::write(journal.join(".record.tmp.123.456.abcdef"), "partial").unwrap();

        recover(&checkout).unwrap();

        assert_eq!(
            fs::read_to_string(checkout.join("managed")).unwrap(),
            "preserve"
        );
        assert!(!journal.exists());
    }

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = crate::test_support::temp_dir(&format!("shdeps-repo-transition-{name}"));
        assert!(dir.metadata().unwrap().dev() > 0);
        dir
    }
}
