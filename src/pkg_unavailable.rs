//! Packages the last full package scan found unavailable on this host.
//!
//! `update` treats a package its manager cannot offer (an older distro
//! release without it) as skipped, not failed, and exits 0. `shdeps health`
//! must not then warn that the package's command is missing, but it may not
//! ask the manager itself. This record carries update's verdict to it.
//!
//! Every full package scan rewrites the whole record (or removes it when
//! nothing was unavailable); a package-cache hit leaves it alone because a
//! hit implies the last full scan was clean, and a quiet run without sudo
//! carries an entry forward because it never re-checked availability. Entries are keyed by dependency
//! name and remember the resolved package, so a config edit that names
//! another package stops matching until the next scan.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::platform::RuntimeEnv;
use crate::state;

const VERSION: &str = "shdeps-pkg-unavailable-v1";
const FILE: &str = "pkg-unavailable";
/// Far larger than any real config; a bigger file is not ours to trust.
const MAX_BYTES: u64 = 1 << 20;

/// Record path inside a state directory.
pub(crate) fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

/// Replaces the record with `unavailable` (dependency name, package) pairs,
/// stamped with the manager and runtime identity they were checked under.
pub(crate) fn write(
    state_dir: &Path,
    env: &RuntimeEnv,
    pkg_mgr: &str,
    unavailable: &BTreeMap<String, String>,
) -> Result<()> {
    let record = path(state_dir);
    if unavailable.is_empty() {
        return match fs::remove_file(&record) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
    }
    let mut content = identity(env, pkg_mgr);
    for (name, package) in unavailable {
        content.push_str(&format!("{name}\t{package}\n"));
    }
    state::write_atomic(&record, &content)
}

/// Dependency name to package for every package the last full scan under
/// this manager and runtime found unavailable. Empty when there is no
/// usable record (absent, unreadable, oversized, or another identity).
#[must_use]
pub(crate) fn read(state_dir: &Path, env: &RuntimeEnv, pkg_mgr: &str) -> BTreeMap<String, String> {
    let Ok(content) = state::read_private_bounded(&path(state_dir), MAX_BYTES) else {
        return BTreeMap::new();
    };
    let Ok(content) = String::from_utf8(content) else {
        return BTreeMap::new();
    };
    let Some(body) = content.strip_prefix(&identity(env, pkg_mgr)) else {
        return BTreeMap::new();
    };
    body.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, package)| (name.to_owned(), package.to_owned()))
        .collect()
}

/// Header lines that must match exactly for the record to apply.
fn identity(env: &RuntimeEnv, pkg_mgr: &str) -> String {
    format!(
        "version\t{VERSION}\npkg_mgr\t{pkg_mgr}\nplatform\t{}\nandroid\t{}\nhost\t{}\n",
        env.platform(),
        u8::from(env.is_android()),
        env.host()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::{path, read, write};
    use crate::platform::RuntimeEnv;
    use crate::test_support::temp_dir;

    #[test]
    fn round_trips_under_the_same_identity_only() {
        let dir = temp_dir("pkg-unavailable-round-trip");
        let env = RuntimeEnv::new("linux", "host-a");
        let unavailable = BTreeMap::from([("eza".to_owned(), "eza".to_owned())]);

        write(&dir, &env, "apt", &unavailable).unwrap();

        assert_eq!(read(&dir, &env, "apt"), unavailable);
        assert!(read(&dir, &env, "dnf").is_empty());
        assert!(read(&dir, &RuntimeEnv::new("linux", "host-b"), "apt").is_empty());
        assert!(read(&dir, &env.clone().with_android(true), "apt").is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_scan_removes_the_record() {
        let dir = temp_dir("pkg-unavailable-empty");
        let env = RuntimeEnv::new("linux", "host-a");
        let unavailable = BTreeMap::from([("eza".to_owned(), "eza".to_owned())]);
        write(&dir, &env, "apt", &unavailable).unwrap();

        write(&dir, &env, "apt", &BTreeMap::new()).unwrap();
        write(&dir, &env, "apt", &BTreeMap::new()).unwrap();

        assert!(!path(&dir).exists());
        assert!(read(&dir, &env, "apt").is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
