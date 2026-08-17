//! Public command symlink helpers.
//!
//! Several install methods produce a binary under a managed install root and
//! expose it through `SHDEPS_BIN_DIR`. The ownership rule is subtle: shdeps may
//! replace its own symlink, but it must not overwrite a regular file the user
//! placed in that command path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::link_state::{self, Kind, ReconcileLink};

/// Result of trying to expose one binary in the public bin directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// The source binary was missing or not executable, so no link was made.
    MissingSource,
    /// A non-symlink already exists at the public command path and was preserved.
    Preserved(PathBuf),
    /// The public symlink was created or replaced.
    Linked(PathBuf),
}

/// Links an executable source binary into `bin_dir` as `cmd`.
///
/// Uses the shared `extras::replace_symlink` helper for the actual
/// staging+atomic-rename so the public-bin path and the man-page /
/// completion paths cannot drift in their TOCTOU semantics. Without
/// this consolidation the public-bin link used a wider
/// `remove_file` → `symlink` window in which the path was momentarily
/// missing entirely; under the state lock no shdeps↔shdeps race could
/// observe that window, but an external tool concurrently writing
/// into `~/.local/bin` could. The shared helper closes that gap.
pub fn one(bin_dir: &Path, cmd: &str, source: &Path) -> Result<Link> {
    if !crate::process::executable_path(source) {
        return Ok(Link::MissingSource);
    }

    fs::create_dir_all(bin_dir)?;
    let target = bin_dir.join(cmd);

    #[cfg(unix)]
    {
        // Use if-else as an expression so neither branch needs a
        // `return` — clippy's `needless_return` lint flags trailing
        // `return` inside a function-final block.
        if crate::extras::replace_symlink(source, &target)? {
            Ok(Link::Linked(target))
        } else {
            Ok(Link::Preserved(target))
        }
    }
    #[cfg(not(unix))]
    {
        // Non-Unix targets are not a supported install platform. We
        // cannot honestly return `Link::Linked` here without an
        // actual symlink call — the `Linked` variant's docstring
        // says "the public symlink was created or replaced" and a
        // no-op return would silently lie to callers that branch on
        // the variant. Preserve any existing entry and otherwise
        // report `MissingSource` so non-Unix debug builds neither
        // stomp user files nor falsely advertise an install.
        if target.exists() {
            Ok(Link::Preserved(target))
        } else {
            Ok(Link::MissingSource)
        }
    }
}

/// Links every executable directly under `install_dir/bin`.
///
/// Repo dependencies may expose several public commands. Tracking the created
/// symlinks lets a later update remove stale commands when the repo changes
/// its bin directory contents.
pub fn from_dir(
    state_dir: &Path,
    bin_dir: &Path,
    name: &str,
    install_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let state_path = link_state::path(state_dir, name, Kind::Bin);
    let source_dir = install_dir.join("bin");
    let entries = match fs::read_dir(&source_dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    // Build the complete desired command inventory before changing any public
    // path. A malformed or unreadable repo `bin` directory must not make a
    // previously working command disappear merely because scanning failed.
    let mut sources = Vec::new();
    if let Some(entries) = entries {
        for entry in entries {
            let path = entry?.path();
            if !crate::process::executable_path(&path) {
                continue;
            }
            let Some(cmd) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            sources.push((cmd.to_owned(), path));
        }
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));

    if !sources.is_empty() {
        fs::create_dir_all(bin_dir)?;
    }
    let mut planned = Vec::new();
    for (cmd, source) in &sources {
        let target = bin_dir.join(cmd);
        // Record every desired pair. Recovery still claims ownership only when
        // the live path is an exact symlink to this source, so a regular client
        // adapter remains unowned. Recording it closes the opposite race: if
        // that regular file disappears and `one` creates the desired symlink,
        // a crash must not leave the new link invisible to prune.
        planned.push(ReconcileLink::new(target, source.clone()));
    }
    let tracked = link_state::begin_reconcile(&state_path, &planned)?;
    let mut created = Vec::new();
    for (cmd, path) in sources {
        if let Link::Linked(link) = one(bin_dir, &cmd, &path)? {
            created.push(link);
        }
    }

    created.sort();
    created.dedup();
    let desired = created
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    // This consumes the prepublication record only after it has converted the
    // exact live desired links into a superset recovery ledger. Prune calls the
    // same recovery path, so ownership remains discoverable even if the dep is
    // removed from config before another update runs.
    link_state::recover_reconcile(&state_path)?;

    for stale in tracked.iter().filter(|path| !desired.contains(*path)) {
        if is_symlink(stale) {
            fs::remove_file(stale)?;
        }
    }
    link_state::write(&state_path, &created)?;
    Ok(created)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::{Link, one};
    use crate::link_state::{self, Kind, ReconcileLink};

    #[test]
    #[cfg(unix)]
    fn link_creates_public_symlink_for_executable_source() {
        let dir = temp_dir("create");
        let source = dir.join("share/tool/bin/tool");
        write_executable(&source);

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Linked(dir.join("bin/tool")));
        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
    }

    #[test]
    #[cfg(unix)]
    fn link_replaces_existing_symlink() {
        let dir = temp_dir("replace");
        let old = dir.join("old");
        let source = dir.join("new");
        write_executable(&old);
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        std::os::unix::fs::symlink(&old, dir.join("bin/tool")).unwrap();

        one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
    }

    #[test]
    fn link_preserves_regular_file_command() {
        let dir = temp_dir("preserve");
        let source = dir.join("source");
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/tool"), "user-owned").unwrap();

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Preserved(dir.join("bin/tool")));
        assert_eq!(
            fs::read_to_string(dir.join("bin/tool")).unwrap(),
            "user-owned"
        );
    }

    #[test]
    fn link_skips_missing_or_non_executable_source() {
        let dir = temp_dir("missing");
        let source = dir.join("source");

        assert_eq!(
            one(&dir.join("bin"), "tool", &source).unwrap(),
            Link::MissingSource
        );

        fs::write(&source, "not executable").unwrap();
        assert_eq!(
            one(&dir.join("bin"), "tool", &source).unwrap(),
            Link::MissingSource
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_replaces_dangling_symlink_via_atomic_rename() {
        // Regression for the bin_link / extras TOCTOU asymmetry. A
        // dangling symlink at the target path must still be treated
        // as shdeps-owned and replaced (not preserved as a stranger
        // file), and the replacement must succeed via the shared
        // staging+rename helper rather than the older delete-then-
        // create path. The observable invariant: at no point does
        // the target path become absent during replacement — a
        // `symlink_metadata` probe taken between the old and new
        // links would observe one of them, never NotFound.
        let dir = temp_dir("atomic-rename");
        let source = dir.join("real-source");
        let bogus = dir.join("does-not-exist");
        write_executable(&source);
        fs::create_dir_all(dir.join("bin")).unwrap();
        // Stage a dangling symlink that points at a non-existent path.
        std::os::unix::fs::symlink(&bogus, dir.join("bin/tool")).unwrap();
        assert!(fs::symlink_metadata(dir.join("bin/tool")).is_ok());
        assert!(fs::metadata(dir.join("bin/tool")).is_err()); // dangling

        let result = one(&dir.join("bin"), "tool", &source).unwrap();

        assert_eq!(result, Link::Linked(dir.join("bin/tool")));
        // The new symlink resolves and the dangling one is gone.
        assert_eq!(fs::read_link(dir.join("bin/tool")).unwrap(), source);
        // And no `.tool.shdeps-link.<pid>.<stamp>` staging file got
        // left behind in the parent — staging-rename's success path
        // moves the staging entry to the target, not abandons it.
        let leftover_staging: Vec<_> = fs::read_dir(dir.join("bin"))
            .unwrap()
            .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
            .filter(|n| n.starts_with(".tool.shdeps-link."))
            .collect();
        assert!(
            leftover_staging.is_empty(),
            "no staging file should remain after successful rename"
        );
    }

    #[test]
    #[cfg(unix)]
    fn link_from_dir_tracks_multiple_repo_commands_and_removes_stale_links() {
        let dir = temp_dir("from-dir");
        let install = dir.join("share/repo");
        let bin_dir = dir.join("bin");
        write_executable(&install.join("bin/tool-a"));
        write_executable(&install.join("bin/tool-b"));
        write_executable(&install.join("bin/not-direct/nested"));

        let created =
            super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

        assert_eq!(created.len(), 2);
        assert_eq!(
            fs::read_link(bin_dir.join("tool-a")).unwrap(),
            install.join("bin/tool-a")
        );
        assert_eq!(
            fs::read_link(bin_dir.join("tool-b")).unwrap(),
            install.join("bin/tool-b")
        );

        fs::remove_file(install.join("bin/tool-b")).unwrap();
        let created =
            super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

        assert_eq!(created, [bin_dir.join("tool-a")]);
        assert!(fs::symlink_metadata(bin_dir.join("tool-b")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn link_from_dir_scan_failure_preserves_prior_links_and_state() {
        let dir = temp_dir("scan-failure");
        let install = dir.join("share/repo");
        let bin_dir = dir.join("bin");
        let public = bin_dir.join("tool");
        let prior_source = dir.join("prior/tool");
        let state = link_state::path(&dir.join("state"), "owner/repo", Kind::Bin);
        write_executable(&prior_source);
        fs::create_dir_all(&bin_dir).unwrap();
        symlink(&prior_source, &public).unwrap();
        link_state::write(&state, std::slice::from_ref(&public)).unwrap();
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("bin"), "not a directory").unwrap();
        let state_before = fs::read(&state).unwrap();

        let error =
            super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap_err();

        assert!(error.to_string().contains("Not a directory"));
        assert_eq!(fs::read_link(&public).unwrap(), prior_source);
        assert_eq!(fs::read(&state).unwrap(), state_before);
    }

    #[test]
    #[cfg(unix)]
    fn link_from_dir_retires_stale_ownership_without_touching_regular_adapter() {
        let dir = temp_dir("regular-adapter");
        let install = dir.join("share/repo");
        let bin_dir = dir.join("bin");
        let public = bin_dir.join("tool");
        let state = link_state::path(&dir.join("state"), "owner/repo", Kind::Bin);
        write_executable(&install.join("bin/tool"));
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(&public, "#!/bin/sh\nexec client-owned-adapter \"$@\"\n").unwrap();
        let mut permissions = fs::metadata(&public).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&public, permissions).unwrap();
        link_state::write(&state, std::slice::from_ref(&public)).unwrap();
        let before = fs::metadata(&public).unwrap();
        let bytes = fs::read(&public).unwrap();

        let owned = super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

        let after = fs::metadata(&public).unwrap();
        assert!(owned.is_empty());
        assert_eq!(fs::read(&public).unwrap(), bytes);
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        assert!(!state.exists());
    }

    #[test]
    #[cfg(unix)]
    fn link_from_dir_recovers_each_durable_reconciliation_phase() {
        for phase in [
            "prepublication",
            "desired-live",
            "union-ledger",
            "stale-retired",
        ] {
            let dir = temp_dir(&format!("recovery-{phase}"));
            let install = dir.join("share/repo");
            let bin_dir = dir.join("bin");
            let desired = bin_dir.join("tool");
            let stale = bin_dir.join("old-tool");
            let desired_source = install.join("bin/tool");
            let stale_source = dir.join("old/tool");
            let state = link_state::path(&dir.join("state"), "owner/repo", Kind::Bin);
            write_executable(&desired_source);
            write_executable(&stale_source);
            fs::create_dir_all(&bin_dir).unwrap();
            symlink(&stale_source, &stale).unwrap();
            link_state::write(&state, std::slice::from_ref(&stale)).unwrap();
            link_state::begin_reconcile(
                &state,
                &[ReconcileLink::new(desired.clone(), desired_source.clone())],
            )
            .unwrap();
            if phase != "prepublication" {
                symlink(&desired_source, &desired).unwrap();
            }
            if phase == "union-ledger" || phase == "stale-retired" {
                link_state::recover_reconcile(&state).unwrap();
            }
            if phase == "stale-retired" {
                fs::remove_file(&stale).unwrap();
            }

            let owned =
                super::from_dir(&dir.join("state"), &bin_dir, "owner/repo", &install).unwrap();

            assert_eq!(
                owned.as_slice(),
                std::slice::from_ref(&desired),
                "phase {phase}"
            );
            assert_eq!(
                fs::read_link(&desired).unwrap(),
                desired_source,
                "phase {phase}"
            );
            assert!(fs::symlink_metadata(&stale).is_err(), "phase {phase}");
            assert_eq!(
                link_state::read(&state).unwrap(),
                [desired],
                "phase {phase}"
            );
        }
    }

    fn write_executable(path: &PathBuf) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-bin-link-{name}"))
    }
}
