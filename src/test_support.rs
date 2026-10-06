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

/// Rewrites the JSON journal at `path` as if it had been recorded while the
/// filesystem was numbered one higher, which is how a journal written before
/// a renumbering reboot reads afterwards: every recorded `device` names
/// another number while every inode stays. A recorded managed-root tree
/// fingerprint is recomputed for that numbering too, since it hashes each
/// entry's device. Returns how many devices moved, so a test cannot silently
/// renumber nothing.
///
/// The rewrite keeps the file's inode, mode, and single link, which journal
/// readers require. Its mtime becomes now, which, like the original write,
/// still follows the birth of every object the journal recorded.
#[cfg(unix)]
pub(crate) fn renumber_journal_devices(path: &std::path::Path) -> usize {
    use serde_json::Value;

    fn renumber(value: &mut Value) -> usize {
        match value {
            Value::Object(fields) => {
                let renumbered = fields
                    .iter_mut()
                    .map(|(key, field)| match field.as_u64() {
                        Some(device) if key == "device" => {
                            *field = Value::from(device + 1);
                            1
                        }
                        _ => renumber(field),
                    })
                    .sum();
                // `cleanup::Evidence` names its managed root beside the
                // root's identity.
                let root = fields
                    .get("managed_install_root")
                    .and_then(Value::as_str)
                    .map(std::path::PathBuf::from);
                let identity = fields.get_mut("managed_install_root_identity");
                if let (Some(root), Some(Value::Object(identity))) = (root, identity) {
                    if identity.get("tree_fingerprint").is_some_and(Value::is_u64) {
                        let recorded = identity["device"].as_u64().unwrap();
                        let devices = crate::tree_fingerprint::Renumbering {
                            live: recorded - 1,
                            recorded,
                        };
                        let fingerprint = crate::tree_fingerprint::of(&root, devices).unwrap();
                        identity.insert("tree_fingerprint".to_owned(), Value::from(fingerprint));
                    }
                }
                renumbered
            }
            Value::Array(items) => items.iter_mut().map(renumber).sum(),
            _ => 0,
        }
    }

    let mut journal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let renumbered = renumber(&mut journal);
    let mut encoded = serde_json::to_vec_pretty(&journal).unwrap();
    encoded.push(b'\n');
    fs::write(path, encoded).unwrap();
    renumbered
}

/// Sets the modification time of the journal at `path`, to place it before
/// or after the birth of an object it records.
#[cfg(unix)]
pub(crate) fn set_journal_time(path: &std::path::Path, time: std::time::SystemTime) {
    fs::File::open(path)
        .and_then(|file| file.set_modified(time))
        .unwrap();
}

/// Writes an executable fixture at `path` that a subprocess will exec.
///
/// Linux refuses to exec a file while any process holds it open for writing
/// (ETXTBSY). An in-process `fs::write` opens such an fd, and a sibling test
/// thread that forks during that window hands its child a copy. Rust's
/// `O_CLOEXEC` does not help: it closes the fd only when that child execs,
/// which a loaded scheduler or a `pre_exec` hook can delay past our own exec
/// of the fixture, so bash intermittently reports "Text file busy" (exit
/// 126). The content therefore goes to a sibling `.src` file that is never
/// exec'd, and a `cp` child creates the executable: `cp` holds the only
/// write fd and exits before we continue. `set_permissions` chmods by path
/// without opening an fd.
#[cfg(unix)]
pub(crate) fn write_executable(path: &std::path::Path, content: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;

    let mut source = path.as_os_str().to_owned();
    source.push(".src");
    let source = PathBuf::from(source);
    fs::write(&source, content).unwrap();
    let copied = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .expect("cp must run to materialize an executable fixture");
    assert!(
        copied.success(),
        "cp failed to materialize {}: {copied}",
        path.display()
    );
    fs::remove_file(&source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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
    // The subprocess runs a full signal-injection flow whose teardown scans
    // the whole process table several times: `ps` plus per-PID probes on
    // macOS, every /proc entry on Linux. Both scale with how busy the host
    // is, and a loaded Linux host with thousands of processes overran the
    // old 5s ceiling just as loaded macOS runners did. This only bounds a
    // true hang, so one generous ceiling serves every platform.
    let budget = Duration::from_secs(30);
    let status = loop {
        if child.exited().unwrap() {
            break Some(child.wait().unwrap());
        }
        if started.elapsed() >= budget {
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
            "signal-boundary subprocess for {test_name} did not exit within {budget:?} and was killed"
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

    #[cfg(unix)]
    #[test]
    fn write_executable_leaves_only_a_runnable_0755_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("shdeps-test-support-executable");
        let path = dir.join("tool");
        super::write_executable(&path, "#!/bin/sh\nprintf 'ran\\n'\n");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(!dir.join("tool.src").exists(), "staging source leaked");
        let output = std::process::Command::new(&path).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"ran\n");
    }
}
