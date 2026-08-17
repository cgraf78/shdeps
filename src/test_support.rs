//! Shared lifecycle for unit-test fixture directories.
//!
//! The process-wide counter avoids timestamp collisions on macOS filesystems
//! with coarse clock resolution. Each directory belongs to its creating test
//! thread and is removed when that thread exits, including panic unwinds.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDirs(Vec<PathBuf>);

impl Drop for TempDirs {
    fn drop(&mut self) {
        for dir in self.0.iter().rev() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

thread_local! {
    static TEMP_DIRS: RefCell<TempDirs> = const { RefCell::new(TempDirs(Vec::new())) };
}

/// Creates a unique fixture directory owned by the current test thread.
pub(crate) fn temp_dir(prefix: &str) -> PathBuf {
    create_temp_dir(&std::env::temp_dir(), prefix)
}

/// Creates a short fixture directory suitable for Unix-domain socket paths.
#[cfg(unix)]
pub(crate) fn short_temp_dir() -> PathBuf {
    create_temp_dir(&std::env::temp_dir(), "s")
}

fn create_temp_dir(parent: &std::path::Path, prefix: &str) -> PathBuf {
    let requested = parent.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&requested);
    fs::create_dir_all(&requested).unwrap();
    // macOS spells its temporary root as /var while the filesystem resolves
    // the same directory through /private/var. Return one physical spelling so
    // fixtures and the production path-normalization code compare the same
    // identity on every supported platform.
    let dir = fs::canonicalize(&requested).unwrap();
    TEMP_DIRS.with(|dirs| dirs.borrow_mut().0.push(dir.clone()));
    dir
}

#[cfg(test)]
mod tests {
    use super::temp_dir;

    #[test]
    fn removes_temp_dirs_when_the_owning_thread_exits() {
        let dir = std::thread::spawn(|| temp_dir("shdeps-test-support-cleanup"))
            .join()
            .unwrap();

        assert!(
            !dir.exists(),
            "temporary test directory leaked: {}",
            dir.display()
        );
    }

    #[test]
    fn returns_the_physical_fixture_path() {
        let dir = temp_dir("shdeps-test-support-physical");

        assert_eq!(dir, std::fs::canonicalize(&dir).unwrap());
    }
}
