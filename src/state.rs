//! Shared state-file helpers.
//!
//! `shdeps` state remains intentionally human-readable, but writes should not
//! be human-fragile. Centralizing atomic replacement here keeps manifest,
//! stamp, link-state, and future cache writers from each inventing slightly
//! different temp-file behavior.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Result;

#[cfg(unix)]
use std::os::fd::AsRawFd;

const LOCK_FILE: &str = ".lock";

/// Per-state-directory advisory lock.
///
/// Hold this only around short read-modify-write windows. The lock exists to
/// keep Rust-owned state files coherent when two `shdeps update` or `prune`
/// runs overlap; it must not be held across package-manager installs, network
/// downloads, or hooks because those operations can block for a long time or
/// call back into shdeps.
#[derive(Debug)]
pub struct StateLock {
    file: File,
}

impl StateLock {
    /// Acquires the per-state-dir lock, waiting for any current holder.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        acquire_impl(state_dir, LockMode::Blocking)
            .map(|lock| lock.expect("blocking state lock acquisition always returns a lock"))
    }

    /// Attempts to acquire the per-state-dir lock without waiting.
    ///
    /// This is primarily useful for tests and future diagnostics. Normal update
    /// code should use the blocking variant so concurrent invocations serialize
    /// instead of failing spuriously on a busy developer machine.
    pub fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        acquire_impl(state_dir, LockMode::NonBlocking)
    }

    /// Returns the lock file path for a state directory.
    #[must_use]
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(LOCK_FILE)
    }
}

/// Replaces a state file with `content` using a same-directory temp file.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp = temp_path(path);
    let write_result = (|| -> Result<()> {
        // Keep the temp file beside the destination so rename stays within one
        // filesystem. Readers then observe either the old complete file or the
        // new complete file, never a partial write from a crashed update.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn temp_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");

    path.with_file_name(format!(".{name}.tmp.{}.{stamp}", std::process::id()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockMode {
    Blocking,
    NonBlocking,
}

fn acquire_impl(state_dir: &Path, mode: LockMode) -> Result<Option<StateLock>> {
    fs::create_dir_all(state_dir)?;
    let path = StateLock::path(state_dir);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // The lock file is just a stable inode for `flock`; its contents do
        // not matter, and truncating it on every acquisition would add needless
        // metadata churn to the state directory.
        .truncate(false)
        .open(&path)?;

    match lock_file(&file, mode)? {
        LockResult::Acquired => Ok(Some(StateLock { file })),
        LockResult::WouldBlock => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockResult {
    Acquired,
    WouldBlock,
}

#[cfg(unix)]
fn lock_file(file: &File, mode: LockMode) -> Result<LockResult> {
    let mut operation = LOCK_EX;
    if mode == LockMode::NonBlocking {
        operation |= LOCK_NB;
    }

    // Use the platform `flock(2)` API directly instead of introducing a crate
    // just for one syscall. shdeps supports Unix-like targets (Linux, WSL, and
    // macOS), and keeping the lock implementation here makes the concurrency
    // contract obvious next to the atomic state writer it protects.
    let rc = unsafe { flock(file.as_raw_fd(), operation) };
    if rc == 0 {
        return Ok(LockResult::Acquired);
    }

    let error = std::io::Error::last_os_error();
    if mode == LockMode::NonBlocking && error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(LockResult::WouldBlock)
    } else {
        Err(error.into())
    }
}

#[cfg(unix)]
impl Drop for StateLock {
    fn drop(&mut self) {
        // Unlock failures during Drop cannot be reported, and the OS will close
        // the descriptor immediately after this anyway. Calling `flock(UN)`
        // keeps tests and long-lived processes from holding the lock until file
        // descriptor teardown if future code wraps this guard in another owner.
        let _ = unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
    }
}

#[cfg(unix)]
const LOCK_EX: i32 = 2;
#[cfg(unix)]
const LOCK_NB: i32 = 4;
#[cfg(unix)]
const LOCK_UN: i32 = 8;

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(not(unix))]
fn lock_file(_file: &File, _mode: LockMode) -> Result<LockResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "state locking is only implemented for Unix-like targets",
    )
    .into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{StateLock, write_atomic};

    #[test]
    fn lock_serializes_state_dir_access() {
        let fixture = Fixture::new("lock");

        let lock = StateLock::acquire(&fixture.dir).unwrap();
        assert!(StateLock::path(&fixture.dir).is_file());
        assert!(StateLock::try_acquire(&fixture.dir).unwrap().is_none());

        drop(lock);
        assert!(StateLock::try_acquire(&fixture.dir).unwrap().is_some());
    }

    #[test]
    fn atomic_write_creates_parent_and_replaces_content() {
        let fixture = Fixture::new("atomic");
        let path = fixture.dir.join("nested/state");

        write_atomic(&path, "old\n").unwrap();
        write_atomic(&path, "new\n").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new\n");
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "shdeps-state-{name}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }
}
