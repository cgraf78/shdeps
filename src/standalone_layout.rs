//! Recognition of the cgraf78/actions standalone release-installer layout.
//!
//! Projects that publish their releases with the shared cgraf78/actions
//! release scripts can ship a self-contained `install.sh` for hosts that do
//! not run Shdeps yet. For release slug `<repo>` that installer publishes:
//!
//! ```text
//! <data-home>/cgraf78/.<repo>-standalone/owner     installer ownership marker
//! <data-home>/cgraf78/.<repo>-standalone/releases/<version>-<platform>/
//! <data-home>/cgraf78/.<repo>-standalone/current -> releases/<version>-<platform>
//! <data-home>/cgraf78/<repo>                     -> .<repo>-standalone/current
//! <bin-dir>/<cmd>                                -> <data-home>/cgraf78/<repo>/<binary>
//! ```
//!
//! With default directories, `<data-home>/cgraf78/<repo>` is exactly the root
//! a Shdeps `github:release` archive install of `cgraf78/<repo>` owns. A host
//! bootstrapped by that installer and later managed by Shdeps therefore
//! presents a symlinked root that no Shdeps marker or ledger can prove. The
//! generic archive classifier correctly refuses such roots, which left those
//! hosts unable to update.
//!
//! This module is the narrow, explicit handshake that resolves that case. It
//! recognizes the layout only when every object is provably the installer's:
//! no-follow checks on each path, exact relative link targets that stay inside
//! the installer's private control directory, the installer's ownership
//! marker, release metadata naming this exact repository, files owned by the
//! current user, and no installer run in progress. Anything else stays
//! unrecognized so callers keep failing closed. The same facts are asserted on
//! the producer side by cgraf78/actions' `test/release-installer-test`.
//!
//! Adoption itself is the normal staged archive switch
//! (`github_release_install::switch_root`), which replaces a symlinked root
//! without exposing a missing root where the filesystem can swap two paths
//! atomically. Otherwise it first re-points a public command symlink at the
//! active release and parks the root link under a fixed name; this module
//! recognizes the state such a fallback switch leaves if it is interrupted so
//! the next update finishes the adoption.
//!
//! The private control directory is deliberately left in place after adoption.
//! A `dot update` that triggered the adoption is itself still executing from
//! that release tree and may read its own packaged files after the Tools
//! stage, so deleting it mid-run could break the very process performing the
//! migration. Once the public command points into the adopted root, nothing
//! Shdeps publishes refers to the directory and it is safe to delete.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// Exact marker the cgraf78/actions release installer writes to
/// `<control>/owner`. The installer refuses to touch a control directory
/// without it, and Shdeps uses the same evidence before adopting one.
#[cfg(unix)]
const INSTALLER_OWNER_MARKER: &str = "cgraf78/actions release-installer v1\n";

/// Release metadata schema written by the shared release packaging.
#[cfg(unix)]
const INSTALL_METADATA_SCHEMA: u64 = 1;

/// Release metadata files are tiny; refuse to parse anything implausibly
/// large rather than reading an arbitrary user file into memory.
#[cfg(unix)]
const INSTALL_METADATA_MAX_BYTES: u64 = 64 * 1024;

/// How the root for one `github:release` dependency relates to the standalone
/// release installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Standalone {
    /// Not provably the installer's layout; normal fail-closed rules apply.
    None,
    /// The installer's own layout (or an interrupted adoption of it) that an
    /// archive update may replace.
    Adoptable,
    /// The installer's layout, but its publication lock exists: an install is
    /// running, or one was killed and left the lock behind. Never interpreted.
    #[cfg(unix)]
    Locked(PathBuf),
}

/// Classifies the Shdeps root for `name` against the standalone layout.
///
/// Classification is a probe: an unreadable or unexpected entry anywhere makes
/// the result [`Standalone::None`] rather than an error, so the caller falls
/// back to its existing fail-closed refusal instead of aborting the run.
#[cfg(unix)]
pub(crate) fn classify(install_base: &Path, name: &str, public: &Path) -> Standalone {
    probe(install_base, name, public).unwrap_or(Standalone::None)
}

/// The installer is a Bash script for POSIX hosts; other platforms never have
/// its layout.
#[cfg(not(unix))]
pub(crate) fn classify(_install_base: &Path, _name: &str, _public: &Path) -> Standalone {
    Standalone::None
}

#[cfg(unix)]
fn probe(install_base: &Path, name: &str, public: &Path) -> std::io::Result<Standalone> {
    use std::fs;
    use std::io;

    let Some((owner, repo)) = split_name(name) else {
        return Ok(Standalone::None);
    };
    let owner_dir = install_base.join(owner);
    let control = owner_dir.join(format!(".{repo}-standalone"));
    // The installer requires its parent and control directory to be real
    // directories. A symlink at either level could redirect every later check
    // to arbitrary content, so treat it as foreign.
    if !owned_entry(&owner_dir, EntryKind::Dir)? || !owned_entry(&control, EntryKind::Dir)? {
        return Ok(Standalone::None);
    }
    let owner_file = control.join("owner");
    if !owned_entry(&owner_file, EntryKind::File)?
        || fs::symlink_metadata(&owner_file)?.len() != INSTALLER_OWNER_MARKER.len() as u64
        || fs::read(&owner_file)? != INSTALLER_OWNER_MARKER.as_bytes()
    {
        return Ok(Standalone::None);
    }
    // The installer holds `lock` for its whole publication. While it exists
    // the `current` link may be mid-switch, so never interpret the tree. A
    // killed installer leaves it behind too; report that distinctly so the
    // owner sees what blocks the update.
    let lock = control.join("lock");
    if entry_exists(&lock)? {
        return Ok(Standalone::Locked(lock));
    }

    let current = control.join("current");
    if !fs::symlink_metadata(&current)?.file_type().is_symlink() {
        return Ok(Standalone::None);
    }
    let current_target = fs::read_link(&current)?;
    let Some(release) = single_child(&current_target, "releases") else {
        return Ok(Standalone::None);
    };
    let releases = control.join("releases");
    let release_dir = releases.join(release);
    if !owned_entry(&releases, EntryKind::Dir)? || !owned_entry(&release_dir, EntryKind::Dir)? {
        return Ok(Standalone::None);
    }
    if !metadata_names_release(&release_dir.join(format!(".{repo}-install.json")), name)? {
        return Ok(Standalone::None);
    }

    // The stable root is either the installer's exact relative link or, after
    // an interrupted fallback switch, moved aside. A parked link proves the
    // latter only when it still carries the installer's exact target. Any
    // other root entry (a real directory, a link elsewhere) is someone else's.
    let link_target = format!(".{repo}-standalone/current");
    let root = install_base.join(name);
    let root_evidence = match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(&root)? != Path::new(&link_target) {
                return Ok(Standalone::None);
            }
            true
        }
        Ok(_) => return Ok(Standalone::None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parked = crate::github_release_install::parked_root_link(&root);
            fs::symlink_metadata(&parked).is_ok_and(|metadata| metadata.is_symlink())
                && fs::read_link(&parked)? == Path::new(&link_target)
        }
        Err(error) => return Err(error),
    };

    let adoptable = match fs::symlink_metadata(public) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // Only a command that resolves into the active release is the
            // installer's (or a fallback switch's pin). A link anywhere else is
            // a user choice that adoption must not silently replace.
            fs::canonicalize(public)?.starts_with(fs::canonicalize(&release_dir)?)
        }
        // A regular launcher in front of the installer's root is preserved by
        // the archive switch. Without that root evidence it is just a raw
        // binary, which is a genuine format change, not this layout.
        Ok(metadata) if metadata.file_type().is_file() => root_evidence,
        Ok(_) => false,
        // The installer may publish its command elsewhere (Termux uses
        // `$PREFIX/bin`). With no root evidence and no public command left,
        // though, there is nothing to adopt: the next install is an ordinary
        // fresh one.
        Err(error) if error.kind() == io::ErrorKind::NotFound => root_evidence,
        Err(error) => return Err(error),
    };
    Ok(if adoptable {
        Standalone::Adoptable
    } else {
        Standalone::None
    })
}

/// Splits a GitHub `owner/repo` name into two plain path components.
#[cfg(unix)]
fn split_name(name: &str) -> Option<(&str, &str)> {
    let (owner, repo) = name.split_once('/')?;
    (plain_component(owner) && plain_component(repo)).then_some((owner, repo))
}

/// Returns `child` when `link` is exactly `<parent>/<child>` with one plain
/// child component, so the target cannot climb out of the control directory.
#[cfg(unix)]
fn single_child<'a>(link: &'a Path, parent: &str) -> Option<&'a str> {
    let text = link.to_str()?;
    let child = text.strip_prefix(parent)?.strip_prefix('/')?;
    plain_component(child).then_some(child)
}

#[cfg(unix)]
fn plain_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains('/')
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum EntryKind {
    Dir,
    File,
}

/// No-follow check that `path` is a real entry of `kind` owned by this user.
#[cfg(unix)]
fn owned_entry(path: &Path, kind: EntryKind) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let kind_matches = match kind {
        EntryKind::Dir => metadata.file_type().is_dir(),
        EntryKind::File => metadata.file_type().is_file(),
    };
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    Ok(kind_matches && metadata.uid() == unsafe { libc::geteuid() })
}

/// No-follow existence check; a dangling symlink still counts as present.
#[cfg(unix)]
fn entry_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Checks that the release's packaged metadata identifies a release install of
/// exactly this repository with the schema Shdeps understands.
#[cfg(unix)]
fn metadata_names_release(path: &Path, name: &str) -> std::io::Result<bool> {
    if !owned_entry(path, EntryKind::File)?
        || std::fs::symlink_metadata(path)?.len() > INSTALL_METADATA_MAX_BYTES
    {
        return Ok(false);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(path)?) else {
        return Ok(false);
    };
    Ok(
        value.get("schema").and_then(serde_json::Value::as_u64) == Some(INSTALL_METADATA_SCHEMA)
            && value.get("method").and_then(serde_json::Value::as_str) == Some("release")
            && value.get("repo").and_then(serde_json::Value::as_str) == Some(name),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use super::{Standalone, classify};
    use crate::test_support::{temp_dir, write_executable};

    const RELEASE: &str = "20260929-074025-4fd04934-linux-x86_64-musl";

    /// Builds the exact tree `install.sh` from cgraf78/actions publishes for
    /// `cgraf78/dot`, returning `(install_base, public_command)`.
    fn standalone(root: &Path) -> (PathBuf, PathBuf) {
        let base = root.join("share");
        let bin = root.join("bin");
        let control = base.join("cgraf78/.dot-standalone");
        let release = control.join("releases").join(RELEASE);
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            control.join("owner"),
            "cgraf78/actions release-installer v1\n",
        )
        .unwrap();
        write_executable(&release.join("dot"), "#!/bin/sh\necho old\n");
        fs::write(
            release.join(".dot-install.json"),
            r#"{
  "schema": 1,
  "method": "release",
  "artifact_platform": "linux-x86_64-musl",
  "version": "20260929-074025-4fd04934",
  "tag": "20260929-074025-4fd04934",
  "commit": "4fd04934e7ae09ff0f2a3556220a2e30987f5e0c",
  "repo": "cgraf78/dot"
}
"#,
        )
        .unwrap();
        symlink(format!("releases/{RELEASE}"), control.join("current")).unwrap();
        symlink(".dot-standalone/current", base.join("cgraf78/dot")).unwrap();
        symlink(base.join("cgraf78/dot/dot"), bin.join("dot")).unwrap();
        (base, bin.join("dot"))
    }

    fn release_dir(base: &Path) -> PathBuf {
        base.join("cgraf78/.dot-standalone/releases").join(RELEASE)
    }

    fn assert_none(base: &Path, public: &Path, why: &str) {
        assert_eq!(
            classify(base, "cgraf78/dot", public),
            Standalone::None,
            "{why}"
        );
    }

    #[test]
    fn recognizes_the_installer_layout() {
        let root = temp_dir("standalone-detect");
        let (base, public) = standalone(&root);

        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable
        );
    }

    #[test]
    fn recognizes_a_pinned_command_with_the_root_link_gone() {
        // Public command already pointing straight at the active release (a
        // fallback switch's pin) while the root link is gone entirely, for
        // example after someone removed the parked link by hand.
        let root = temp_dir("standalone-interrupted");
        let (base, public) = standalone(&root);
        fs::remove_file(base.join("cgraf78/dot")).unwrap();
        fs::remove_file(&public).unwrap();
        symlink(
            fs::canonicalize(release_dir(&base).join("dot")).unwrap(),
            &public,
        )
        .unwrap();

        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable
        );
    }

    #[test]
    fn recognizes_a_parked_root_link_for_any_public_command() {
        // A fallback switch killed between its renames leaves the root link
        // under the fixed parked name. Launchers and Termux-style absent
        // commands must still converge on the next update.
        let root = temp_dir("standalone-parked");
        let (base, public) = standalone(&root);
        let link = base.join("cgraf78/dot");
        fs::rename(
            &link,
            crate::github_release_install::parked_root_link(&link),
        )
        .unwrap();
        fs::remove_file(&public).unwrap();
        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable,
            "absent public command"
        );

        write_executable(&public, "#!/bin/sh\nexec launcher\n");
        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable,
            "regular launcher"
        );
    }

    #[test]
    fn rejects_a_parked_link_with_another_target() {
        let root = temp_dir("standalone-parked-foreign");
        let (base, public) = standalone(&root);
        let link = base.join("cgraf78/dot");
        fs::remove_file(&link).unwrap();
        symlink(
            release_dir(&base),
            crate::github_release_install::parked_root_link(&link),
        )
        .unwrap();
        fs::remove_file(&public).unwrap();
        write_executable(&public, "#!/bin/sh\nexec launcher\n");

        assert_none(&base, &public, "only the installer's exact target counts");
    }

    #[test]
    fn recognizes_a_regular_launcher_in_front_of_the_root_link() {
        let root = temp_dir("standalone-launcher");
        let (base, public) = standalone(&root);
        fs::remove_file(&public).unwrap();
        write_executable(&public, "#!/bin/sh\nexec launcher\n");

        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable
        );
    }

    #[test]
    fn recognizes_the_root_link_when_the_command_lives_elsewhere() {
        // Termux publishes the installer's command under `$PREFIX/bin`, so the
        // Shdeps public path is absent while the root link still exists.
        let root = temp_dir("standalone-no-public");
        let (base, public) = standalone(&root);
        fs::remove_file(&public).unwrap();

        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Adoptable
        );
    }

    #[test]
    fn reports_an_installer_lock_without_interpreting_the_tree() {
        let root = temp_dir("standalone-locked");
        let (base, public) = standalone(&root);
        let lock = base.join("cgraf78/.dot-standalone/lock");
        fs::create_dir(&lock).unwrap();

        assert_eq!(
            classify(&base, "cgraf78/dot", &public),
            Standalone::Locked(lock)
        );
    }

    #[test]
    fn rejects_a_missing_root_without_a_command_into_the_release() {
        let root = temp_dir("standalone-nothing-to-adopt");
        let (base, public) = standalone(&root);
        fs::remove_file(base.join("cgraf78/dot")).unwrap();
        fs::remove_file(&public).unwrap();

        assert_none(&base, &public, "nothing is left to adopt");
    }

    #[test]
    fn rejects_a_raw_public_binary_without_the_root_link() {
        let root = temp_dir("standalone-raw-public");
        let (base, public) = standalone(&root);
        fs::remove_file(base.join("cgraf78/dot")).unwrap();
        fs::remove_file(&public).unwrap();
        write_executable(&public, "#!/bin/sh\necho raw\n");

        assert_none(&base, &public, "a raw binary is a real format change");
    }

    #[test]
    fn rejects_a_public_link_outside_the_active_release() {
        let root = temp_dir("standalone-foreign-public");
        let (base, public) = standalone(&root);
        let other = root.join("dev/dot");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        write_executable(&other, "#!/bin/sh\necho dev\n");
        fs::remove_file(&public).unwrap();
        symlink(&other, &public).unwrap();

        assert_none(&base, &public, "a user-chosen command link is foreign");
    }

    #[test]
    fn rejects_a_root_link_with_any_other_target() {
        let root = temp_dir("standalone-foreign-root");
        let (base, public) = standalone(&root);
        let link = base.join("cgraf78/dot");
        fs::remove_file(&link).unwrap();
        // Resolves to the same directory, but not through the installer's
        // exact relative spelling: a user made this link, not the installer.
        symlink(release_dir(&base), &link).unwrap();

        assert_none(&base, &public, "only the installer's exact link counts");
    }

    #[test]
    fn rejects_a_real_root_directory() {
        let root = temp_dir("standalone-real-root");
        let (base, public) = standalone(&root);
        fs::remove_file(base.join("cgraf78/dot")).unwrap();
        fs::create_dir(base.join("cgraf78/dot")).unwrap();

        assert_none(&base, &public, "a real root is not the installer's link");
    }

    #[test]
    fn rejects_a_missing_or_different_owner_marker() {
        let root = temp_dir("standalone-owner-marker");
        let (base, public) = standalone(&root);
        let owner = base.join("cgraf78/.dot-standalone/owner");

        fs::write(&owner, "cgraf78/actions release-installer v2\n").unwrap();
        assert_none(&base, &public, "another installer generation");

        fs::write(&owner, "cgraf78/actions release-installer v1").unwrap();
        assert_none(&base, &public, "marker bytes must match exactly");

        fs::remove_file(&owner).unwrap();
        symlink(root.join("elsewhere"), &owner).unwrap();
        assert_none(&base, &public, "a symlinked marker is never followed");
    }

    #[test]
    fn rejects_a_current_link_that_escapes_the_releases_directory() {
        let root = temp_dir("standalone-escaping-current");
        let (base, public) = standalone(&root);
        let current = base.join("cgraf78/.dot-standalone/current");
        for target in ["releases/../owner", "../dot-standalone-copy", "releases"] {
            fs::remove_file(&current).unwrap();
            symlink(target, &current).unwrap();
            assert_none(&base, &public, &format!("current -> {target}"));
        }
    }

    #[test]
    fn rejects_symlinked_control_releases_or_release_directories() {
        for (label, relative) in [
            ("control", "cgraf78/.dot-standalone".to_owned()),
            ("releases", "cgraf78/.dot-standalone/releases".to_owned()),
            (
                "release",
                format!("cgraf78/.dot-standalone/releases/{RELEASE}"),
            ),
        ] {
            let root = temp_dir(&format!("standalone-symlinked-{label}"));
            let (base, public) = standalone(&root);
            let real = base.join(&relative);
            let moved = root.join(format!("moved-{label}"));
            fs::rename(&real, &moved).unwrap();
            symlink(&moved, &real).unwrap();

            assert_none(&base, &public, &format!("symlinked {label} directory"));
        }
    }

    #[test]
    fn rejects_metadata_for_another_repo_schema_or_method() {
        let root = temp_dir("standalone-metadata");
        let (base, public) = standalone(&root);
        let metadata = release_dir(&base).join(".dot-install.json");
        for body in [
            r#"{"schema":1,"method":"release","repo":"cgraf78/other"}"#,
            r#"{"schema":2,"method":"release","repo":"cgraf78/dot"}"#,
            r#"{"schema":1,"method":"checkout","repo":"cgraf78/dot"}"#,
            "not json",
        ] {
            fs::write(&metadata, body).unwrap();
            assert_none(&base, &public, &format!("metadata {body}"));
        }
        fs::remove_file(&metadata).unwrap();
        assert_none(&base, &public, "missing metadata");
    }

    #[test]
    fn unexpected_entry_types_degrade_to_unrecognized() {
        // `releases` as a regular file makes later probes fail with ENOTDIR;
        // classification must answer "not the installer's", never abort.
        let root = temp_dir("standalone-enotdir");
        let (base, public) = standalone(&root);
        let releases = base.join("cgraf78/.dot-standalone/releases");
        fs::remove_dir_all(&releases).unwrap();
        fs::write(&releases, "not a directory\n").unwrap();

        assert_none(&base, &public, "a file where a directory belongs");
    }

    #[test]
    fn rejects_names_that_are_not_two_plain_components() {
        let root = temp_dir("standalone-names");
        let (base, public) = standalone(&root);
        for name in ["dot", "cgraf78/dot/extra", "../dot", "cgraf78/.."] {
            assert_eq!(
                classify(&base, name, &public),
                Standalone::None,
                "{name} must not be interpreted"
            );
        }
    }
}
