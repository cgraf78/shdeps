//! Durable package-manager ownership proofs.
//!
//! Command presence alone cannot prove that the configured package owns a
//! tool: another package or an older Shdeps provider may expose the same name.
//! A small dependency-scoped record lets first adoption verify package-manager
//! ownership once while preserving the command-only warm path afterward.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::state;

const VERSION: &str = "shdeps-pkg-proof-v1";
const MAX_PROOF_BYTES: u64 = 4096;

pub(crate) fn path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(format!("{name}.pkg-proof"))
}

pub(crate) fn current(
    state_dir: &Path,
    name: &str,
    manager: &str,
    package: &str,
    command: &str,
) -> bool {
    state::read_private_bounded(&path(state_dir, name), MAX_PROOF_BYTES)
        .ok()
        .and_then(|record| String::from_utf8(record).ok())
        .is_some_and(|record| record == content(manager, package, command))
}

pub(crate) fn write(
    state_dir: &Path,
    name: &str,
    manager: &str,
    package: &str,
    command: &str,
) -> Result<()> {
    state::write_atomic(&path(state_dir, name), &content(manager, package, command))
}

pub(crate) fn remove(state_dir: &Path, name: &str) -> Result<()> {
    let proof = path(state_dir, name);
    match fs::remove_file(&proof) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    let mut parent = proof.parent();
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

fn content(manager: &str, package: &str, command: &str) -> String {
    format!("{VERSION}\nmanager={manager}\npackage={package}\ncommand={command}\n")
}

#[cfg(test)]
mod tests {
    use super::{current, path, remove, write};

    #[test]
    fn proof_round_trip_binds_manager_package_and_command() {
        let state = crate::test_support::temp_dir("shdeps-package-proof");

        write(&state, "owner/tool", "apt", "tool-package", "tool").unwrap();

        assert!(current(&state, "owner/tool", "apt", "tool-package", "tool"));
        assert!(!current(&state, "owner/tool", "apt", "replacement", "tool"));
        assert!(path(&state, "owner/tool").is_file());

        remove(&state, "owner/tool").unwrap();
        assert!(!path(&state, "owner/tool").exists());
    }

    #[test]
    #[cfg(unix)]
    fn proof_reader_rejects_symlinked_state() {
        use std::os::unix::fs::symlink;

        let state = crate::test_support::temp_dir("shdeps-package-proof-symlink");
        write(&state, "source", "apt", "tool-package", "tool").unwrap();
        let linked = path(&state, "linked");
        symlink(path(&state, "source"), &linked).unwrap();

        assert!(!current(&state, "linked", "apt", "tool-package", "tool"));
    }
}
