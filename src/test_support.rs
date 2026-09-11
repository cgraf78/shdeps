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
#[cfg(unix)]
static NEXT_SUBPROCESS_MARKER: AtomicU64 = AtomicU64::new(0);

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

/// Runs a test-fixture subprocess through the production ownership boundary.
///
/// An environment marker is not visible until exec. Registration before fork
/// therefore matters when another test concurrently proves its boundary empty.
#[cfg(unix)]
pub(crate) fn run_subprocess(
    mut command: std::process::Command,
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;

    command
        .env(
            "SHDEPS_INTERNAL_PROCESS_BOUNDARIES",
            format!(
                "test-fixture-{}-{}",
                std::process::id(),
                NEXT_SUBPROCESS_MARKER.fetch_add(1, Ordering::Relaxed)
            ),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawn_guard = crate::cancellation::test_spawn_guard()?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let registration = crate::cancellation::test_spawn_registration();
    let child = command.spawn();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let child_registration = child
        .as_ref()
        .ok()
        .map(|child| crate::cancellation::test_child_registration(child.id()));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    drop(registration);
    drop(spawn_guard);
    let output = child?.wait_with_output();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    drop(child_registration);
    output
}

/// Install a last-words backtrace hook for signal-boundary children.
///
/// The parent pings a stuck child with SIGUSR2 before its kill deadline so
/// the child's stderr shows where it wedged. Best-effort only: capture may
/// deadlock against a lock the stuck thread already holds, in which case
/// the parent's kill proceeds with no trace. Failure-only; success paths
/// never deliver SIGUSR2.
#[cfg(unix)]
pub(crate) fn install_timeout_backtrace_hook() {
    unsafe extern "C" fn dump_backtrace(_signal: i32) {
        let trace = std::backtrace::Backtrace::capture();
        let text = format!("TIMEOUT_BACKTRACE:\n{trace}\n");
        let bytes = text.as_bytes();
        // SAFETY: raw write avoids the stderr lock the stuck thread may hold.
        unsafe {
            libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len());
        }
    }
    // SAFETY: zeroed sigaction with an explicit mask; a failed install just
    // leaves the default disposition, which still ends the stuck child.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = dump_backtrace as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut());
    }
}

/// Runs a signal-injection unit test in its own process.
///
/// The production cancellation latch is intentionally process-global. Rust's
/// unit-test harness runs unrelated tests in parallel in one process, so a
/// test that raises a real signal must not expose that latch to its neighbors.
#[cfg(unix)]
pub(crate) fn run_signal_boundary_subprocess(test_name: &str, child_env: &str) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = crate::cancellation::spawn_owned(
        &mut command,
        crate::cancellation::Isolation::DetachedSession,
        true,
    )
    .unwrap();
    let started = Instant::now();
    let status = loop {
        if child.exited().unwrap() {
            break Some(child.wait().unwrap());
        }
        if started.elapsed() >= Duration::from_secs(5) {
            // Ask a stuck child for its backtrace before killing it. The
            // child installs the hook on entry; a child that never got that
            // far dies to the default disposition, which still ends the wait.
            // SAFETY: the retained child PID is live (it has not exited).
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGUSR2);
            }
            std::thread::sleep(Duration::from_millis(300));
            let _ = child.stop(crate::cancellation::KILL_SIGNAL);
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    match status {
        Some(status) if status.success() => {}
        Some(status) => {
            panic!("signal-boundary subprocess for {test_name} exited without success: {status}")
        }
        None => panic!(
            "signal-boundary subprocess for {test_name} did not exit within 5s and was killed"
        ),
    }
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
