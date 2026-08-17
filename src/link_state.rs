//! Tracked symlink state for public command and extras links.
//!
//! `shdeps` writes `.binlinks` and `.links` files so relink, prune, and method
//! transitions can remove stale symlinks later. The state file lists paths that
//! shdeps created; cleanup still checks that each path is a symlink before
//! removing it so a user-owned regular file at the same location is preserved.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::state;

const RECONCILE_FORMAT: &str = "shdeps link reconciliation v1";
const RECONCILE_SUFFIX: &str = "reconcile-v1";
const MAX_RECONCILE_BYTES: u64 = 4 * 1024 * 1024;

/// One desired symlink which may become Shdeps-owned during reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileLink {
    path: PathBuf,
    target: PathBuf,
}

impl ReconcileLink {
    /// Records the exact live link and target pair recovery may claim.
    pub(crate) fn new(path: PathBuf, target: PathBuf) -> Self {
        Self { path, target }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileRecord {
    format: String,
    state_path: PathBuf,
    prior: Vec<PathBuf>,
    desired: Vec<ReconcileLink>,
}

/// Link-state file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Public command symlinks under `SHDEPS_BIN_DIR`.
    Bin,
    /// Extra symlinks such as man pages and shell completions.
    Extras,
}

impl Kind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Bin => "binlinks",
            Self::Extras => "links",
        }
    }
}

/// Returns the link-state path for a dependency name.
#[must_use]
pub fn path(state_dir: &Path, name: &str, kind: Kind) -> PathBuf {
    state_dir.join(format!("{name}.{}", kind.suffix()))
}

/// Reads tracked links. Missing link-state files are empty state.
pub fn read(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Writes tracked links, removing the state file when there is nothing to track.
///
/// Returns `InvalidInput` if any link path contains a newline (`\n`)
/// OR a carriage return (`\r`). The on-disk format is newline-
/// delimited, so an embedded `\n` would split a single path into two
/// phantom paths on the next `read` — silently corrupting cleanup
/// state. A trailing `\r` is rejected on the same grounds: it would
/// survive `str::lines` parsing as part of the path and create a
/// stored value that no later filesystem call can match. POSIX
/// permits both characters in filenames but no real install method
/// shdeps drives produces them; surfacing this as an error catches
/// a misbehaving hook or upstream archive rather than letting the
/// bad data round-trip.
pub fn write(path: &Path, links: &[PathBuf]) -> Result<()> {
    if links.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }

    let mut content = String::new();
    for link in links {
        let display = link.to_string_lossy();
        if display.contains('\n') || display.contains('\r') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to record link path containing newline: {}",
                    display.escape_default()
                ),
            )
            .into());
        }
        content.push_str(&display);
        content.push('\n');
    }
    state::write_atomic(path, &content)
}

/// Publishes durable recovery authority before desired links become live.
///
/// The returned paths are the prior ownership snapshot used by the caller to
/// retire stale links only after desired publication and recovery-state commit.
pub(crate) fn begin_reconcile(path: &Path, desired: &[ReconcileLink]) -> Result<Vec<PathBuf>> {
    recover_reconcile(path)?;
    let prior = read(path)?;
    validate_paths(
        prior
            .iter()
            .chain(desired.iter().flat_map(|link| [&link.path, &link.target])),
    )?;

    // Avoid creating an otherwise empty transaction for the common
    // no-command/no-prior-state case.
    if prior.is_empty() && desired.is_empty() {
        return Ok(prior);
    }

    let record = ReconcileRecord {
        format: RECONCILE_FORMAT.to_owned(),
        state_path: path.to_path_buf(),
        prior: prior.clone(),
        desired: desired.to_vec(),
    };
    let mut encoded = serde_json::to_string_pretty(&record)?;
    encoded.push('\n');
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_RECONCILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "link reconciliation plan exceeds the supported record size",
        )
        .into());
    }
    state::write_atomic(&reconcile_path(path), &encoded)?;
    Ok(prior)
}

/// Recovers prepublication ownership independently of current configuration.
///
/// This is intentionally called by both relink and cleanup. If a process died
/// after publishing a new desired link but before updating `.binlinks`, prune
/// must still discover and remove that exact Shdeps-created symlink.
pub(crate) fn recover_reconcile(path: &Path) -> Result<()> {
    let transaction = reconcile_path(path);
    match fs::symlink_metadata(&transaction) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }

    let bytes = crate::state::read_private_bounded(&transaction, MAX_RECONCILE_BYTES)?;
    let record: ReconcileRecord = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed link reconciliation record: {error}"),
        )
    })?;
    if record.format != RECONCILE_FORMAT || record.state_path != path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "link reconciliation record does not belong to {}",
                path.display()
            ),
        )
        .into());
    }
    validate_paths(
        record.prior.iter().chain(
            record
                .desired
                .iter()
                .flat_map(|link| [&link.path, &link.target]),
        ),
    )?;

    let mut recovered = BTreeSet::new();
    for prior in record.prior {
        if is_symlink(&prior) {
            recovered.insert(prior);
        }
    }
    for desired in record.desired {
        if fs::symlink_metadata(&desired.path).is_ok_and(|metadata| {
            metadata.file_type().is_symlink()
                && fs::read_link(&desired.path).is_ok_and(|target| target == desired.target)
        }) {
            recovered.insert(desired.path);
        }
    }
    write(path, &recovered.into_iter().collect::<Vec<_>>())?;
    fs::remove_file(transaction)?;
    Ok(())
}

/// Removes symlinks listed in a link-state file, then removes the state file.
pub fn unlink_tracked(path: &Path) -> Result<()> {
    unlink_tracked_matching(path, |link| {
        if fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            fs::remove_file(link)?;
            Ok(true)
        } else {
            Ok(false)
        }
    })
    .map(|_| ())
}

/// Runs an ownership-aware unlink operation, then retires the state file.
///
/// Cleanup callers use this to prove that a live symlink still belongs to the
/// dependency whose historical path ledger mentioned it. A later install may
/// legitimately retarget the same public command before the old dependency is
/// pruned; path-only state must not grant authority over that replacement.
pub(crate) fn unlink_tracked_matching(
    path: &Path,
    mut unlink: impl FnMut(&Path) -> Result<bool>,
) -> Result<Vec<PathBuf>> {
    recover_reconcile(path)?;
    let mut removed = Vec::new();
    for link in read(path)? {
        if unlink(&link)? {
            removed.push(link);
        }
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(removed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(removed),
        Err(error) => Err(error.into()),
    }
}

fn reconcile_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("links");
    path.with_file_name(format!("{name}.{RECONCILE_SUFFIX}"))
}

fn validate_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Result<()> {
    for path in paths {
        let display = path.to_string_lossy();
        if display.contains('\n') || display.contains('\r') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to reconcile link path containing newline: {}",
                    display.escape_default()
                ),
            )
            .into());
        }
    }
    Ok(())
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        Kind, ReconcileLink, begin_reconcile, path, read, reconcile_path, recover_reconcile,
        unlink_tracked, write,
    };

    #[test]
    fn paths_preserve_nested_dependency_names() {
        let state = PathBuf::from("/tmp/state");

        assert_eq!(
            path(&state, "owner/tool", Kind::Bin),
            PathBuf::from("/tmp/state/owner/tool.binlinks")
        );
        assert_eq!(
            path(&state, "owner/tool", Kind::Extras),
            PathBuf::from("/tmp/state/owner/tool.links")
        );
    }

    #[test]
    fn read_missing_state_as_empty() {
        let dir = temp_dir("missing");

        assert_eq!(
            read(&dir.join("missing.links")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn write_and_read_link_state() {
        let dir = temp_dir("round-trip");
        let state = path(&dir, "owner/tool", Kind::Extras);
        let links = [dir.join("man/man1/tool.1"), dir.join("zsh/_tool")];

        write(&state, &links).unwrap();

        assert_eq!(read(&state).unwrap(), links);
    }

    #[test]
    fn write_refuses_link_paths_containing_newline() {
        // The on-disk format is newline-delimited. A path with an
        // embedded `\n` would survive a write/read cycle as two
        // phantom paths, silently corrupting prune state — at best
        // leaving stale symlinks, at worst pointing the cleaner at
        // an unrelated path. Surface the malformed input here.
        let dir = temp_dir("newline-link");
        let state = path(&dir, "tool", Kind::Extras);
        let bad = dir.join("share/man\ninjected");
        let err = write(&state, &[bad]).unwrap_err();
        assert!(
            err.to_string().contains("newline"),
            "error should explain the newline rejection: {err}"
        );
        // The state file must NOT exist (atomic-write rolled back).
        assert!(!state.exists());
    }

    #[test]
    fn write_empty_removes_state_file() {
        let dir = temp_dir("empty");
        let state = path(&dir, "tool", Kind::Bin);

        write(&state, &[dir.join("bin/tool")]).unwrap();
        write(&state, &[]).unwrap();

        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn unlink_tracked_removes_symlinks_and_preserves_regular_files() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("unlink");
        let state = path(&dir, "tool", Kind::Bin);
        let target = dir.join("target");
        let symlink_path = dir.join("bin/tool");
        let regular_path = dir.join("bin/regular-tool");
        fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
        fs::write(&target, "target").unwrap();
        symlink(&target, &symlink_path).unwrap();
        fs::write(&regular_path, "user-owned").unwrap();

        write(&state, &[symlink_path.clone(), regular_path.clone()]).unwrap();
        unlink_tracked(&state).unwrap();

        assert!(!symlink_path.exists());
        assert_eq!(fs::read_to_string(&regular_path).unwrap(), "user-owned");
        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn unlink_tracked_removes_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("dangling");
        let state = path(&dir, "tool", Kind::Extras);
        let target = dir.join("missing-target");
        let symlink_path = dir.join("share/man/man1/tool.1");
        fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
        symlink(&target, &symlink_path).unwrap();

        write(&state, std::slice::from_ref(&symlink_path)).unwrap();
        unlink_tracked(&state).unwrap();

        assert!(fs::symlink_metadata(&symlink_path).is_err());
        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_record_makes_prepublished_links_visible_to_cleanup() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("reconcile-cleanup");
        let state = path(&dir, "tool", Kind::Bin);
        let old_target = dir.join("old-target");
        let new_target = dir.join("new-target");
        let old_link = dir.join("bin/old");
        let new_link = dir.join("bin/new");
        fs::create_dir_all(old_link.parent().unwrap()).unwrap();
        fs::write(&old_target, "old").unwrap();
        fs::write(&new_target, "new").unwrap();
        symlink(&old_target, &old_link).unwrap();
        write(&state, std::slice::from_ref(&old_link)).unwrap();
        begin_reconcile(
            &state,
            &[ReconcileLink::new(new_link.clone(), new_target.clone())],
        )
        .unwrap();
        // This is the crash boundary which the old delete/relink sequence
        // could not recover: a desired command is live, but final `.binlinks`
        // ownership has not been written yet.
        symlink(&new_target, &new_link).unwrap();

        unlink_tracked(&state).unwrap();

        assert!(fs::symlink_metadata(&old_link).is_err());
        assert!(fs::symlink_metadata(&new_link).is_err());
        assert!(!state.exists());
        assert!(!reconcile_path(&state).exists());
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_before_publication_preserves_prior_ownership_only() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("reconcile-before-publication");
        let state = path(&dir, "tool", Kind::Bin);
        let old_target = dir.join("old-target");
        let new_target = dir.join("new-target");
        let old_link = dir.join("bin/old");
        let new_link = dir.join("bin/new");
        fs::create_dir_all(old_link.parent().unwrap()).unwrap();
        fs::write(&old_target, "old").unwrap();
        fs::write(&new_target, "new").unwrap();
        symlink(&old_target, &old_link).unwrap();
        write(&state, std::slice::from_ref(&old_link)).unwrap();
        begin_reconcile(&state, &[ReconcileLink::new(new_link, new_target)]).unwrap();

        recover_reconcile(&state).unwrap();

        assert_eq!(read(&state).unwrap(), [old_link]);
        assert!(!reconcile_path(&state).exists());
    }

    #[test]
    fn oversized_reconciliation_plan_is_rejected_before_publication() {
        let dir = temp_dir("reconcile-oversized");
        let state = path(&dir, "tool", Kind::Bin);
        let oversized = PathBuf::from("x".repeat((super::MAX_RECONCILE_BYTES + 1) as usize));

        let error = begin_reconcile(&state, &[ReconcileLink::new(oversized, dir.join("target"))])
            .unwrap_err();

        assert!(error.to_string().contains("exceeds"));
        assert!(!reconcile_path(&state).exists());
        assert!(!state.exists());
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-link-state-{name}"))
    }
}
