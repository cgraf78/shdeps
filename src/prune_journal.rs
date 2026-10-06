//! Durable pre-hook cleanup authority for `shdeps prune`.
//!
//! Prune captures filesystem identity (`cleanup::Evidence`) before running an
//! orphan's arbitrary `uninstall()` hook so built-in cleanup can tell the
//! orphan's own artifacts from replacements the hook installs. Kept only in
//! memory, that snapshot died with the process: a signal or crash after the
//! hook (or a failed hook that prune keeps for a retry) left the manifest row
//! behind, and the next prune re-captured evidence from the hook's
//! replacement files and deleted them as orphan-owned.
//!
//! This module persists one record per orphan before its hook runs and marks
//! it once the hook succeeds. A later prune reuses the original evidence and
//! does not rerun a hook that already completed. The record is retired only
//! after the manifest row is gone. Records live in a private directory under
//! the state dir and are only read or written while the state lock is held.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

use crate::Result;
use crate::cleanup::{self, Evidence};
use crate::manifest::{Manifest, ManifestEntry};

const DIR: &str = ".prune-hooks-v1";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// Cleanup authority for one orphan whose prune has started.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    /// Exact manifest row the evidence was captured for.
    pub(crate) entry: ManifestEntry,
    /// Filesystem identity captured before the uninstall hook ran.
    pub(crate) evidence: Evidence,
    /// Whether `uninstall()` already returned success for this row.
    pub(crate) hook_completed: bool,
}

/// Returns the record for `entry`, or `None` when no prune of this exact row
/// has started.
///
/// A record for the same name but a different row belongs to an earlier
/// install that was since replaced; its evidence says nothing about the
/// current artifacts, so it is ignored (and overwritten by the next save).
pub(crate) fn load(roots: &cleanup::Roots, entry: &ManifestEntry) -> Result<Option<Record>> {
    let dir = dir(roots);
    match fs::symlink_metadata(&dir) {
        Ok(_) => validate_dir(&dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let path = record_path(roots, &entry.name);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let record = read(&path)?;
    Ok((record.entry == *entry).then_some(record))
}

/// Durably publishes `record`, replacing any earlier record for its name.
///
/// The write is deliberately not cancellable: callers publish right after a
/// hook returns, and a latched signal must not discard the only evidence of
/// that hook's completion.
pub(crate) fn save(roots: &cleanup::Roots, record: &Record) -> Result<()> {
    ensure_dir(roots)?;
    let mut encoded = serde_json::to_string_pretty(record)?;
    encoded.push('\n');
    crate::state::write_atomic(&record_path(roots, &record.entry.name), &encoded)
}

/// Retires the record for `name` after its manifest row was removed.
pub(crate) fn remove(roots: &cleanup::Roots, name: &str) -> Result<()> {
    match fs::remove_file(record_path(roots, name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Drops records whose manifest row is gone or was rewritten.
///
/// A crash between manifest removal and record retirement, or a reinstall
/// that rewrote the row, leaves a record whose evidence no longer describes
/// the tracked artifacts. A record is deliberately kept while its exact row
/// survives, even if the row is not an orphan on this run: a transiently
/// re-added config entry or an unknown host identity must not discard the
/// only pre-hook snapshot. The residual risk of keeping it (a reinstall that
/// reproduces the identical row) is a skipped hook and preserved files, never
/// a deletion, because the stale evidence cannot match new file identities.
pub(crate) fn retain(roots: &cleanup::Roots, manifest: &Manifest) -> Result<()> {
    let dir = dir(roots);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    validate_dir(&dir)?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            // `state::write_atomic` temp files orphaned by a SIGKILL mid-write
            // carry no authority; anything else here is also ours to clear.
            fs::remove_file(&path)?;
            continue;
        }
        let record = read(&path)?;
        if manifest.get(&record.entry.name) != Some(&record.entry) {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Record> {
    let bytes = crate::state::read_private_bounded(path, MAX_RECORD_BYTES)?;
    let mut record: Record = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed prune record {}: {error}", path.display()),
        )
    })?;
    record.evidence.bind_to_journal(path);
    Ok(record)
}

fn dir(roots: &cleanup::Roots) -> PathBuf {
    dir_path(&roots.state_dir)
}

/// Returns the record directory; a non-empty one means a prune is unfinished.
pub(crate) fn dir_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR)
}

// Dependency names contain `/`; hashing keeps one flat file per name.
fn record_path(roots: &cleanup::Roots, name: &str) -> PathBuf {
    dir(roots).join(format!(
        "{}.json",
        crate::checksum::sha256_hex(name.as_bytes())
    ))
}

fn ensure_dir(roots: &cleanup::Roots) -> Result<()> {
    let dir = dir(roots);
    fs::create_dir_all(&roots.state_dir)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(&dir) {
        Ok(()) => {
            // Apply the mode explicitly so a permissive umask cannot widen it,
            // and sync the parent so the new directory survives a crash along
            // with the records `state::write_atomic` syncs into it.
            #[cfg(unix)]
            {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                fs::File::open(&roots.state_dir)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => validate_dir(&dir),
        Err(error) => Err(error.into()),
    }
}

// Records authorize deletions, so only a private directory owned by this user
// is trusted; a symlink or shared directory fails closed.
fn validate_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("prune record state is not a directory: {}", path.display()),
        )
        .into());
    }
    #[cfg(unix)]
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "prune record state has unsafe ownership or mode: {}",
                path.display()
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Record, load, remove, retain, save};
    use crate::cleanup::{Evidence, Roots};
    use crate::manifest::{Manifest, ManifestEntry};

    fn roots(name: &str) -> Roots {
        let root = crate::test_support::temp_dir(&format!("shdeps-prune-journal-{name}"));
        Roots {
            state_dir: root.join("state"),
            install_dir: root.join("share"),
            bin_dir: root.join("bin"),
        }
    }

    fn record(entry: &ManifestEntry) -> Record {
        Record {
            entry: entry.clone(),
            evidence: Evidence::default(),
            hook_completed: true,
        }
    }

    #[test]
    fn saved_record_round_trips_only_for_the_same_row() {
        let roots = roots("round-trip");
        let entry = ManifestEntry::new("owner/tool", "github:release", "tool", "/bin/tool");
        save(&roots, &record(&entry)).unwrap();

        assert_eq!(load(&roots, &entry).unwrap(), Some(record(&entry)));
        let reinstalled = ManifestEntry::new("owner/tool", "github:release", "tool", "/x/tool");
        assert_eq!(load(&roots, &reinstalled).unwrap(), None);

        remove(&roots, &entry.name).unwrap();
        assert_eq!(load(&roots, &entry).unwrap(), None);
        remove(&roots, &entry.name).unwrap();
    }

    #[test]
    fn retain_keeps_records_only_while_their_exact_row_survives() {
        let roots = roots("retain");
        let kept = ManifestEntry::new("kept", "custom", "kept", "");
        let removed = ManifestEntry::new("removed", "custom", "removed", "");
        let rewritten = ManifestEntry::new("owner/tool", "github:release", "tool", "/old");
        for entry in [&kept, &removed, &rewritten] {
            save(&roots, &record(entry)).unwrap();
        }
        let manifest = Manifest::parse("kept|custom|kept|\nowner/tool|github:release|tool|/new\n");

        retain(&roots, &manifest).unwrap();

        assert!(load(&roots, &kept).unwrap().is_some());
        assert!(load(&roots, &removed).unwrap().is_none());
        assert!(load(&roots, &rewritten).unwrap().is_none());
        assert!(
            std::fs::read_dir(roots.state_dir.join(super::DIR))
                .unwrap()
                .count()
                == 1
        );
    }

    #[test]
    #[cfg(unix)]
    fn shared_record_directory_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let roots = roots("shared-dir");
        let entry = ManifestEntry::new("tool", "custom", "tool", "");
        save(&roots, &record(&entry)).unwrap();
        let dir = roots.state_dir.join(super::DIR);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        assert!(load(&roots, &entry).is_err());
        assert!(save(&roots, &record(&entry)).is_err());
    }

    #[test]
    fn malformed_record_fails_closed() {
        let roots = roots("malformed");
        let entry = ManifestEntry::new("tool", "custom", "tool", "");
        save(&roots, &record(&entry)).unwrap();
        std::fs::write(super::record_path(&roots, &entry.name), "{").unwrap();

        assert!(load(&roots, &entry).is_err());
        assert!(retain(&roots, &Manifest::default()).is_err());
    }
}
