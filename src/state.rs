//! Shared state-file helpers.
//!
//! `shdeps` state remains intentionally human-readable, but writes should not
//! be human-fragile. Centralizing atomic replacement here keeps manifest,
//! stamp, link-state, and future cache writers from each inventing slightly
//! different temp-file behavior.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;

#[cfg(unix)]
use std::os::fd::AsRawFd;

const LOCK_FILE: &str = ".lock";
const LOCK_TIMEOUT_ENV: &str = "SHDEPS_STATE_LOCK_TIMEOUT_SECS";
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LOCK_WAIT_POLL: Duration = Duration::from_millis(100);

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
///
/// **PID binding:** the env var value is the lock holder's PID, and
/// the reentry guard fires ONLY when the value equals the current
/// process's PARENT pid (`getppid`). This blocks the trivial-bypass
/// case where a user (or hostile shell init) does
/// `export SHDEPS_STATE_LOCK_HELD=1` — that value (or any non-PID
/// integer) would not match `getppid()` for a top-level invocation,
/// so locking is still enforced. The hook-spawn path in `apply_hook_env` writes the
/// parent's `std::process::id()` into the env, which matches the
/// child's `getppid()` exactly when the child was spawned by that
/// parent.
///
/// See `StateLock`'s docstring for the threat model — in particular
/// what this PID-binding does NOT defend against.
pub const REENTRY_ENV: &str = "SHDEPS_STATE_LOCK_HELD";

/// Per-state-directory advisory lock.
///
/// The lock exists to keep Rust-owned state files coherent when two
/// `shdeps update` or `prune` runs overlap. It is intentionally held
/// for the full duration of `update::run` and `prune::run` —
/// including package-manager installs, network downloads, and hooks
/// — because the manifest/link-state writes interleaved through
/// those operations all need the same serialization guarantee.
///
/// **Reentry contract:** a hook subprocess that recursively invokes
/// `shdeps update` / `shdeps prune` (e.g., a `post()` hook that
/// installs another dep) WOULD deadlock against the parent's flock
/// without an escape valve. The escape valve is the `REENTRY_ENV`
/// env var:
///
/// 1. `hooks::apply_hook_env` writes the Rust parent's PID into
///    `SHDEPS_STATE_LOCK_HELD` on every hook subprocess command.
/// 2. Each hook bash script re-exports `SHDEPS_STATE_LOCK_HELD=$$`
///    so the value is rebound to bash's own PID before any
///    recursive `shdeps` invocation. See the SCRIPT constants in
///    `hooks.rs` for the prelude line.
/// 3. `StateLock::acquire` checks the env value against `getppid()`
///    in `is_legitimate_reentry`. When they match (= the inner
///    shdeps's parent is the bash that knows about the outer
///    lock), `acquire` returns a no-op `StateLock { file: None }`
///    instead of trying to re-take the flock.
///
/// Callers spawning ANY subprocess that may recursively invoke
/// shdeps MUST propagate the env var the same way `apply_hook_env`
/// and the hook SCRIPT prelude do — otherwise the recursive child
/// will block on the parent's flock. The `+` prefix is avoided
/// because rustdoc parses it as a Markdown list marker and triggers
/// `clippy::doc_lazy_continuation` on the next line.
///
/// **Threat model — what this guard does NOT defend against:**
/// PID matching is forgeable. A wrapper script that exports
/// `SHDEPS_STATE_LOCK_HELD=$$` before launching shdeps will pass
/// the reentry check (the value matches the child's real `getppid()`)
/// and bypass the flock. This is a deliberate trade-off: the guard
/// is a *deadlock prevention* mechanism for legitimate hook
/// recursion, not a security boundary. The concurrency invariants
/// the lock protects (manifest, link-state, stamp, cache writes)
/// are corrupted only by interleaved writers from the same user
/// account, and that user already has full ability to inspect or
/// break their own state. A truly unforgeable signal would require
/// a per-lock nonce written to a state-dir file (so the env value
/// is verified against state only the lock owner could have
/// written) or fd inheritance of the lock itself; both are larger
/// refactors than the deadlock-prevention contract warrants.
/// Operators sharing a `SHDEPS_STATE_DIR` between less-trusted
/// tooling and shdeps invocations should rely on filesystem
/// permissions and not on this env var as a trust boundary.
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
        if is_legitimate_reentry() {
            return Ok(StateLock { file: None });
        }
        acquire_with_timeout(state_dir, lock_timeout())
    }

    /// Attempts to acquire the per-state-dir lock without waiting.
    ///
    /// This is primarily useful for tests and diagnostics. Normal update code
    /// should use `acquire` so concurrent invocations serialize until the
    /// configured wait budget is exhausted.
    pub fn try_acquire(state_dir: &Path) -> Result<Option<Self>> {
        if is_legitimate_reentry() {
            // Same re-entry rule as `acquire`: report success without
            // actually acquiring so callers gated on `Some(_)` proceed
            // normally.
            return Ok(Some(StateLock { file: None }));
        }
        acquire_impl(state_dir)
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

fn acquire_impl(state_dir: &Path) -> Result<Option<StateLock>> {
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
    set_close_on_exec(&file)?;

    match lock_file(&file)? {
        LockResult::Acquired => {
            write_lock_metadata(&file, state_dir)?;
            Ok(Some(StateLock { file: Some(file) }))
        }
        LockResult::WouldBlock => Ok(None),
    }
}

fn acquire_with_timeout(state_dir: &Path, timeout: Duration) -> Result<StateLock> {
    let started = Instant::now();
    loop {
        if let Some(lock) = acquire_impl(state_dir)? {
            return Ok(lock);
        }
        if started.elapsed() >= timeout {
            return Err(lock_timeout_error(state_dir, timeout).into());
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(std::cmp::min(LOCK_WAIT_POLL, remaining));
    }
}

fn lock_timeout() -> Duration {
    std::env::var(LOCK_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LOCK_TIMEOUT)
}

fn lock_timeout_error(state_dir: &Path, timeout: Duration) -> std::io::Error {
    let path = StateLock::path(state_dir);
    let mut message = format!(
        "timed out after {}s waiting for shdeps state lock at {}",
        timeout.as_secs(),
        path.display()
    );
    if let Ok(owner) = fs::read_to_string(&path) {
        let owner = owner.trim();
        if !owner.is_empty() {
            message.push_str("; current lock metadata: ");
            message.push_str(&owner.replace('\n', ", "));
        }
    }
    std::io::Error::new(std::io::ErrorKind::TimedOut, message)
}

fn write_lock_metadata(mut file: &File, state_dir: &Path) -> Result<()> {
    let acquired_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(
        file,
        "pid={}\nstate_dir={}\nacquired_unix={acquired_unix}\n",
        std::process::id(),
        state_dir.display()
    )?;
    file.sync_data()?;
    Ok(())
}

#[cfg(unix)]
fn set_close_on_exec(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    if flags & libc::FD_CLOEXEC != 0 {
        return Ok(());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_close_on_exec(_file: &File) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockResult {
    Acquired,
    WouldBlock,
}

#[cfg(unix)]
fn lock_file(file: &File) -> Result<LockResult> {
    let operation = libc::LOCK_EX | libc::LOCK_NB;

    // Keep the lock implementation in this module so the concurrency
    // contract sits next to the atomic state writer it protects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if rc == 0 {
        return Ok(LockResult::Acquired);
    }

    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
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
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// True when the current process is a legitimate reentry from a lock-holding
/// parent.
///
/// The env var alone is insufficient: a user could `export
/// SHDEPS_STATE_LOCK_HELD=1` in their shell init and silently disable
/// the lock for every subsequent invocation. Binding the value to the
/// parent PID and verifying it against `getppid()` blocks that bypass:
/// a literal `"1"` value will never match a real PID, and a parent
/// shdeps that legitimately spawned us put its own `process::id()`
/// into the env, which matches the child's `getppid()` exactly. The
/// check is best-effort — on non-Unix targets we conservatively
/// refuse to treat any env value as a valid reentry signal, so the
/// caller goes through the real acquire path (which is itself
/// `cfg(not(unix))` => unsupported error today).
fn is_legitimate_reentry() -> bool {
    #[cfg(unix)]
    {
        let Some(value) = std::env::var_os(REENTRY_ENV) else {
            return false;
        };
        let Some(value) = value.to_str() else {
            return false;
        };
        let Ok(expected_parent_pid) = value.trim().parse::<i32>() else {
            return false;
        };
        // SAFETY: `getppid` is a non-failing POSIX syscall on all
        // Unix targets Rust supports. It takes no arguments and
        // cannot fault.
        let actual_parent = unsafe { libc::getppid() };
        actual_parent == expected_parent_pid && actual_parent > 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
pub(crate) fn lock_reentry_env_for_test() -> std::sync::MutexGuard<'static, ()> {
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
pub(crate) fn clear_reentry_env_for_test() {
    // SAFETY: all tests that call this helper hold `lock_reentry_env_for_test`.
    unsafe {
        std::env::remove_var(REENTRY_ENV);
    }
}

#[cfg(test)]
pub(crate) fn set_reentry_env_for_test(value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: all tests that call this helper hold `lock_reentry_env_for_test`.
    unsafe {
        std::env::set_var(REENTRY_ENV, value);
    }
}

#[cfg(test)]
fn set_lock_timeout_env_for_test(value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: state tests serialize env mutation with `lock_reentry_env_for_test`.
    unsafe {
        std::env::set_var(LOCK_TIMEOUT_ENV, value);
    }
}

#[cfg(test)]
fn clear_lock_timeout_env_for_test() {
    // SAFETY: state tests serialize env mutation with `lock_reentry_env_for_test`.
    unsafe {
        std::env::remove_var(LOCK_TIMEOUT_ENV);
    }
}

#[cfg(not(unix))]
fn lock_file(_file: &File) -> Result<LockResult> {
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
        let _env_guard = super::lock_reentry_env_for_test();
        super::clear_reentry_env_for_test();
        let fixture = Fixture::new("lock");

        let lock = StateLock::acquire(&fixture.dir).unwrap();
        assert!(StateLock::path(&fixture.dir).is_file());
        assert!(StateLock::try_acquire(&fixture.dir).unwrap().is_none());

        drop(lock);
        assert!(StateLock::try_acquire(&fixture.dir).unwrap().is_some());
    }

    #[test]
    fn acquire_times_out_with_lock_metadata_when_holder_stays_alive() {
        let _env_guard = super::lock_reentry_env_for_test();
        super::clear_reentry_env_for_test();
        super::set_lock_timeout_env_for_test("0");
        let _timeout_reset = EnvReset;
        let fixture = Fixture::new("lock-timeout");

        let primary = StateLock::acquire(&fixture.dir).unwrap();
        let error = StateLock::acquire(&fixture.dir).unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("timed out after 0s waiting for shdeps state lock"),
            "timeout should name the lock wait failure: {message}"
        );
        assert!(
            message.contains("pid=") && message.contains("acquired_unix="),
            "timeout should include holder metadata for diagnosis: {message}"
        );

        drop(primary);
        assert!(StateLock::try_acquire(&fixture.dir).unwrap().is_some());
    }

    #[test]
    fn acquired_lock_file_records_owner_metadata() {
        let _env_guard = super::lock_reentry_env_for_test();
        super::clear_reentry_env_for_test();
        let fixture = Fixture::new("lock-metadata");

        let _lock = StateLock::acquire(&fixture.dir).unwrap();
        let metadata = fs::read_to_string(StateLock::path(&fixture.dir)).unwrap();

        assert!(metadata.contains(&format!("pid={}", std::process::id())));
        assert!(metadata.contains(&format!("state_dir={}", fixture.dir.display())));
        assert!(metadata.contains("acquired_unix="));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lock_fd_is_not_inherited_by_execed_children() {
        let _env_guard = super::lock_reentry_env_for_test();
        super::clear_reentry_env_for_test();
        let fixture = Fixture::new("lock-cloexec");
        let _lock = StateLock::acquire(&fixture.dir).unwrap();
        let lock_path = StateLock::path(&fixture.dir);

        let script = r#"
for fd in /proc/self/fd/*; do
  target=$(readlink "$fd" 2>/dev/null || true)
  if [ "$target" = "$LOCK_PATH" ]; then
    exit 7
  fi
done
"#;
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("LOCK_PATH", &lock_path)
            .status()
            .unwrap();

        assert!(
            status.success(),
            "execed children must not inherit the lock fd: {status:?}"
        );
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
        //
        // Tests run in parallel within the same process by default,
        // and `set_var`/`remove_var` mutate process-global state.
        // The shared `ENV_TEST_LOCK` mutex serializes every test in
        // this module that touches `REENTRY_ENV` so concurrent test
        // threads do not see each other's transient values. The
        // mutex is intentionally module-private; callers in other
        // modules that need the same guarantee should use a similar
        // pattern.
        let _env_guard = super::lock_reentry_env_for_test();
        // Defensive reset: if a previous test panicked while holding
        // the env mutated, the recovered (poisoned) mutex hands us
        // control but the env var is still set. Clear it explicitly
        // before any acquire so we never inherit stale state.
        super::clear_reentry_env_for_test();

        let fixture = Fixture::new("reentry");
        let primary = StateLock::acquire(&fixture.dir).unwrap();

        // The reentry env value MUST equal the current process's
        // parent PID (`getppid`) for the guard to fire. Use it
        // directly rather than a literal "1" so the test exercises
        // the PID-binding contract, not just env-presence.
        let parent_pid = unsafe { libc::getppid() };
        super::set_reentry_env_for_test(parent_pid.to_string());
        let reentry = StateLock::acquire(&fixture.dir).unwrap();
        super::clear_reentry_env_for_test();

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
    fn reentry_env_with_wrong_pid_does_not_bypass_lock() {
        // The user-export bypass scenario: a hostile or careless
        // shell init sets `SHDEPS_STATE_LOCK_HELD=1` (or any value
        // not equal to `getppid()`). The reentry guard must NOT
        // fire; locking must still be enforced.
        let _env_guard = super::lock_reentry_env_for_test();
        // Same defensive reset as the sibling reentry test — clear
        // stale env state from any previously-panicking test.
        super::clear_reentry_env_for_test();

        let fixture = Fixture::new("reentry-wrong-pid");
        let primary = StateLock::acquire(&fixture.dir).unwrap();

        // Use a sentinel value that definitely won't match getppid.
        // Both `1` (a literal that a hostile env-export might use)
        // and `0` (invalid PID) should be refused.
        super::set_reentry_env_for_test("1");
        let try_acquire_with_wrong_pid = StateLock::try_acquire(&fixture.dir).unwrap();
        super::clear_reentry_env_for_test();

        assert!(
            try_acquire_with_wrong_pid.is_none(),
            "wrong-PID env value must NOT bypass the lock — primary is still held"
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
            let dir = crate::test_support::temp_dir(&format!("shdeps-state-{name}"));
            Self { dir }
        }
    }

    struct EnvReset;

    impl Drop for EnvReset {
        fn drop(&mut self) {
            super::clear_lock_timeout_env_for_test();
        }
    }
}
