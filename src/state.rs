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

/// Env var set by a lock holder before spawning hook subprocesses.
///
/// When a hook invokes shdeps recursively (e.g., a `post()` hook that
/// runs `shdeps update some-other-dep`), the inner `StateLock::acquire`
/// must NOT try to re-acquire the same `flock` — `flock` is not
/// reentrant for a different file descriptor from the same process
/// tree, and the inner process would deadlock waiting for itself.
/// `SHDEPS_STATE_LOCK_HELD` is the cooperative signal that a parent
/// already holds the lock; the inner acquire returns a no-op guard
/// instead. Hooks inherit the env var transparently, so any depth of
/// re-entry from a single top-level `shdeps update` is covered.
pub const REENTRY_ENV: &str = "SHDEPS_STATE_LOCK_HELD";

/// Per-state-directory advisory lock.
///
/// Hold this only around short read-modify-write windows. The lock exists to
/// keep Rust-owned state files coherent when two `shdeps update` or `prune`
/// runs overlap; it must not be held across package-manager installs, network
/// downloads, or hooks because those operations can block for a long time or
/// call back into shdeps.
#[derive(Debug)]
pub struct StateLock {
    /// `None` means this is a re-entry no-op guard: a parent shdeps
    /// already holds the file lock, so we did not open/lock the file
    /// ourselves and `Drop` does nothing.
    file: Option<File>,
}

impl StateLock {
    /// Acquires the per-state-dir lock, waiting for any current holder.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        if std::env::var_os(REENTRY_ENV).is_some() {
            // Hook re-entry path: the parent shdeps that spawned this
            // subprocess already holds the flock. Acquiring our own
            // would deadlock the parent (it is waiting for the hook
            // to finish, but the hook is waiting for the lock the
            // parent holds). Return a no-op guard.
            return Ok(StateLock { file: None });
        }
        acquire_impl(state_dir, LockMode::Blocking)
            .map(|lock| lock.expect("blocking state lock acquisition always returns a lock"))
    }

    /// Attempts to acquire the per-state-dir lock without waiting.
    ///
    /// This is primarily useful for tests and future diagnostics. Normal update
    /// code should use the blocking variant so concurrent invocations serialize
    /// instead of failing spuriously on a busy developer machine.
    pub fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        if std::env::var_os(REENTRY_ENV).is_some() {
            // Same re-entry rule as `acquire`: report success without
            // actually acquiring so callers gated on `Some(_)` proceed
            // normally.
            return Ok(Some(StateLock { file: None }));
        }
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
    // PID + nanos alone can collide when two threads in the same process
    // write to the same directory in the same nanosecond — `create_new`
    // then surfaces a confusing `AlreadyExists` error and the loser's
    // write fails on what is really a self-collision. Mixing in a nonce
    // pushes the collision probability into the cosmic-noise range so
    // the `create_new` invariant remains a real concurrency guard
    // rather than a TOCTOU trap.
    let nonce = temp_nonce();
    path.with_file_name(format!(
        ".{name}.tmp.{}.{stamp}.{nonce:016x}",
        std::process::id()
    ))
}

/// Returns a 64-bit nonce for atomic-write temp filenames.
///
/// Tries `/dev/urandom` first on Unix because it is the cheapest
/// per-call entropy source available without adding a crate
/// dependency. Falls back to mixing in thread-id, address, and a
/// monotonic counter — still strong enough to make same-nanosecond
/// collisions vanishingly rare.
fn temp_nonce() -> u64 {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 8];
            if f.read_exact(&mut buf).is_ok() {
                return u64::from_ne_bytes(buf);
            }
        }
    }
    // Software fallback: bit-mix several sources that vary between
    // concurrent writers in the same process.
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    // The address of a local heap allocation differs per call thanks
    // to allocator-state churn and ASLR.
    let probe = Box::new(0u8);
    (std::ptr::from_ref::<u8>(&*probe) as usize).hash(&mut hasher);
    hasher.finish()
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
        LockResult::Acquired => Ok(Some(StateLock { file: Some(file) })),
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
        //
        // `self.file` is `None` for re-entry no-op guards (a hook
        // subprocess that inherited `SHDEPS_STATE_LOCK_HELD`); in that
        // case the parent owns the real lock and we have nothing to
        // unlock here.
        if let Some(file) = self.file.as_ref() {
            let _ = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        }
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

    use super::{StateLock, temp_nonce, write_atomic};

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
    fn reentry_env_short_circuits_acquire_to_no_op_guard() {
        // Hook subprocesses inherit `SHDEPS_STATE_LOCK_HELD` from
        // `apply_hook_env`; the inner `acquire` must NOT try to
        // re-take the parent's flock or it deadlocks against itself.
        // Verifying with a real lock held: while a primary holder is
        // alive on this thread, a second `acquire` with the env var
        // set must succeed immediately and produce a no-op guard
        // (the existing holder's lock stays unaffected).
        let fixture = Fixture::new("reentry");
        let primary = StateLock::acquire(&fixture.dir).unwrap();

        // Use `temp_env`-style scoped set/unset: we set the env var,
        // call acquire, and unset before yielding. `set_var` is safe
        // here because the test is single-threaded and we revert.
        // SAFETY: `set_var`/`remove_var` are marked unsafe in recent
        // Rust nightlies but stable as of this writing; single-threaded
        // test context, value reverted before exit.
        unsafe {
            std::env::set_var(super::REENTRY_ENV, "1");
        }
        let reentry = StateLock::acquire(&fixture.dir).unwrap();
        unsafe {
            std::env::remove_var(super::REENTRY_ENV);
        }

        // Both guards exist simultaneously without deadlock — the
        // re-entry guard would have blocked the test thread forever
        // pre-fix.
        drop(reentry);
        // The primary still holds the real lock (re-entry was a no-op).
        assert!(
            StateLock::try_acquire(&fixture.dir).unwrap().is_none(),
            "primary lock must still be held after re-entry guard drops"
        );
        drop(primary);
    }

    #[test]
    fn temp_nonce_is_unique_across_consecutive_calls() {
        // The nonce mixes /dev/urandom or per-thread/per-allocation
        // entropy; either source must produce different values on
        // consecutive calls in the same thread. A duplicate nonce on
        // back-to-back invocations is the exact failure mode that
        // would re-introduce the same-nanosecond temp-filename
        // collision the helper exists to prevent.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            assert!(
                seen.insert(temp_nonce()),
                "temp_nonce returned a duplicate within {} calls",
                seen.len()
            );
        }
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
