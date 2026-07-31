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
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
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
}
