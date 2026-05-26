//! Bash hook subprocess support.
//!
//! Custom dependency hooks are trusted user code, but the Rust port should not
//! source them into the Rust process or model them as ordinary libraries. Running
//! each hook query in a short Bash subprocess preserves the shell API boundary
//! while preventing hook-defined functions from leaking between dependencies.
//!
//! # Hook file layout
//!
//! The hook file for a dep named `<name>` is looked up at
//! `<hooks_dir>/<name>.sh`. For deps named `owner/repo` (the
//! github:repo / github:release convention), the slash is preserved in
//! the path, so the hook file MUST live at
//! `<hooks_dir>/owner/repo.sh` — the `owner/` subdirectory is part of
//! the path. A file placed flat at `<hooks_dir>/repo.sh` will produce
//! a silent `MissingHook` result rather than running. This mirrors the
//! `<install_dir>/<name>` layout the install path uses for the same
//! deps, keeping hooks parallel to their dep's install root.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::config;
use crate::config::Entry;
use crate::runtime::Roots;
use crate::status::CustomProbe;

/// Default wall-clock timeout for a single hook subprocess.
///
/// A misbehaving hook (infinite loop, blocking on a closed pipe, waiting
/// on user input) used to hang the parent `shdeps update` indefinitely.
/// Five minutes is generous for a real install hook that downloads and
/// builds something while still preventing the unbounded-hang failure
/// mode. Operators can override with `SHDEPS_HOOK_TIMEOUT_SECS`.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 300;

/// Default per-stream cap for hook stdout/stderr capture.
///
/// `command.output()` buffers the full child output in memory, so a hook
/// that emits unbounded text (e.g., `version() { cat /var/log/syslog; }`
/// — a real footgun in the wild) could OOM the parent. We continue
/// draining the pipes after the cap so the child does not block on a
/// full pipe buffer, but only the first `MAX` bytes are kept in memory.
const DEFAULT_HOOK_MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Poll interval used while waiting for a hook child to exit.
const HOOK_WAIT_POLL: Duration = Duration::from_millis(50);

/// Runs a configured hook command with a wall-clock timeout and a
/// per-stream output cap.
fn run_hook_command(mut command: Command) -> io::Result<std::process::Output> {
    let timeout = hook_timeout();
    let max_bytes = hook_max_bytes();

    // Put the child in its own process group on Unix so timeout-kill
    // can reach grandchildren too. Without this, `child.kill()` only
    // terminates the immediate bash process; a hook that backgrounds
    // a subprocess (`some-tool &`) leaves that subprocess holding the
    // inherited stdout/stderr pipes open, so the reader threads block
    // on EOF until the grandchild exits — defeating
    // `SHDEPS_HOOK_TIMEOUT_SECS`. With the child as a process-group
    // leader (via `setsid`), `kill(-pgid, SIG)` signals the whole
    // group atomically.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and the pre-exec
        // callback runs between fork and exec where only
        // async-signal-safe calls are valid. Detaching from the
        // parent's controlling terminal is the documented purpose.
        unsafe {
            command.pre_exec(|| {
                if libc_setsid() == -1 {
                    let err = std::io::Error::last_os_error();
                    // EPERM means the caller is already a process-group
                    // leader, so setsid refuses — but the child inherits
                    // that leader status, which is exactly the state we
                    // wanted (kill(-pgid) will reach the whole group).
                    // Any other errno genuinely blocks detachment.
                    if err.raw_os_error() != Some(EPERM) {
                        return Err(err);
                    }
                }
                Ok(())
            });
        }
    }

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain the pipes concurrently in background threads so the child
    // cannot wedge on a full pipe buffer while we wait for it to exit.
    // Each reader stops storing bytes after `max_bytes` but keeps
    // reading-and-discarding until EOF; without that, a runaway hook
    // would block forever on the next pipe write.
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_handle = thread::spawn(move || read_capped(stdout, max_bytes));
    let stderr_handle = thread::spawn(move || read_capped(stderr, max_bytes));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Hook timed out. Signal the whole process group so any
            // grandchildren the hook backgrounded are reaped too —
            // otherwise they keep the inherited pipes open and the
            // reader threads hang past the timeout.
            #[cfg(unix)]
            {
                // The child was placed in its own session by
                // `setsid` above, so its PID is also its PGID. Negate
                // the PID to signal the whole group.
                let pgid = -(child.id() as i32);
                // SAFETY: `kill` with a negative PID is a documented
                // POSIX way to signal a process group. The pgid is
                // the child's own session-leader PID which we just
                // created via setsid in pre_exec; it cannot have
                // been reused while the child is still alive
                // (per `try_wait` above returning None).
                unsafe {
                    libc_kill(pgid, SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
            break child.wait()?;
        }
        thread::sleep(HOOK_WAIT_POLL);
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

// EPERM == 1 on every Unix this crate is built for (Linux, macOS, and
// the BSDs all derive their errno table from the same historical base).
// We avoid pulling in `libc` as a dependency just to surface a single
// constant, mirroring the inline `extern "C"` declarations below.
#[cfg(unix)]
const EPERM: i32 = 1;

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn read_capped<R: Read>(mut reader: R, max_bytes: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => return kept,
            Ok(n) => n,
            Err(_) => return kept,
        };
        if kept.len() < max_bytes {
            // Append only up to the cap; subsequent bytes from this read
            // are intentionally discarded, but the loop continues so the
            // child's later writes do not block on a full pipe buffer.
            let room = max_bytes - kept.len();
            let take = std::cmp::min(room, n);
            kept.extend_from_slice(&chunk[..take]);
        }
    }
}

fn hook_timeout() -> Duration {
    std::env::var("SHDEPS_HOOK_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS))
}

fn hook_max_bytes() -> usize {
    std::env::var("SHDEPS_HOOK_MAX_OUTPUT_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_HOOK_MAX_OUTPUT_BYTES)
}

/// Generated Bash compatibility layer for Rust hook subprocesses.
pub mod prelude;

// Each hook bash script begins with `export SHDEPS_STATE_LOCK_HELD=$$`,
// which re-binds the lock-reentry env var to THIS bash process's PID.
// The parent Rust process wrote the var with its OWN PID via
// `apply_hook_env`, which would make the recursive `shdeps` invocation's
// `getppid()` mismatch — the recursive child's parent is this bash, not
// the Rust grandparent. Without the re-bind, a hook that calls
// `shdeps update` / `shdeps prune` against the same state dir would
// deadlock waiting for the lock its grandparent still holds. With the
// re-bind, the recursive child's `getppid()` equals the new env value
// (= our `$$`), `is_legitimate_reentry` fires, and the lock acquisition
// becomes a no-op guard.
//
// User-export-bypass remains blocked: a user shell init that exported
// `SHDEPS_STATE_LOCK_HELD=1` is overridden by our `$$` re-export when
// their shell goes through a hook, and a top-level `shdeps` invocation
// (no hook in the chain) never sees the bash re-export at all.
const STATUS_SCRIPT: &str = r#"
export SHDEPS_STATE_LOCK_HELD=$$
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
if [[ "$lib" == "__shdeps_inline_prelude__" ]]; then
  [[ -n "${SHDEPS_HOOK_PRELUDE_SOURCE:-}" ]] || exit 1
  eval "$SHDEPS_HOOK_PRELUDE_SOURCE" || exit 1
else
  . "$lib" 2>/dev/null || exit 1
fi
. "$hook" 2>/dev/null || exit 1

declare -f exists >/dev/null 2>&1 || exit 1
exists "$name" >/dev/null 2>&1 || exit 1

if declare -f version >/dev/null 2>&1; then
  version "$name" 2>/dev/null || true
fi
"#;

const UNINSTALL_SCRIPT: &str = r#"
export SHDEPS_STATE_LOCK_HELD=$$
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
if [[ "$lib" == "__shdeps_inline_prelude__" ]]; then
  [[ -n "${SHDEPS_HOOK_PRELUDE_SOURCE:-}" ]] || exit 11
  eval "$SHDEPS_HOOK_PRELUDE_SOURCE" || exit 11
else
  . "$lib" 2>/dev/null || exit 11
fi
. "$hook" 2>/dev/null || exit 12

declare -f uninstall >/dev/null 2>&1 || exit 10
uninstall "$name"
"#;

const INSTALL_SCRIPT: &str = r#"
export SHDEPS_STATE_LOCK_HELD=$$
name=$1
lib=$2
hook=$3
reinstall=$4

unset -f exists version install post uninstall 2>/dev/null || true
if [[ "$lib" == "__shdeps_inline_prelude__" ]]; then
  [[ -n "${SHDEPS_HOOK_PRELUDE_SOURCE:-}" ]] || exit 11
  eval "$SHDEPS_HOOK_PRELUDE_SOURCE" || exit 11
else
  . "$lib" 2>/dev/null || exit 11
fi
. "$hook" 2>/dev/null || exit 12

declare -f exists >/dev/null 2>&1 || exit 10
if exists "$name" >/dev/null 2>&1 && [[ "$reinstall" != 1 ]]; then
  if declare -f version >/dev/null 2>&1; then
    version "$name" 2>/dev/null || true
  fi
  exit 20
fi

declare -f install >/dev/null 2>&1 || exit 13
install "$name" || exit 14
if declare -f version >/dev/null 2>&1; then
  version "$name" 2>/dev/null || true
fi
"#;

const POST_SCRIPT: &str = r#"
export SHDEPS_STATE_LOCK_HELD=$$
name=$1
lib=$2
hook=$3

unset -f exists version install post uninstall 2>/dev/null || true
if [[ "$lib" == "__shdeps_inline_prelude__" ]]; then
  [[ -n "${SHDEPS_HOOK_PRELUDE_SOURCE:-}" ]] || exit 11
  eval "$SHDEPS_HOOK_PRELUDE_SOURCE" || exit 11
else
  . "$lib" 2>/dev/null || exit 11
fi
. "$hook" 2>/dev/null || exit 12

declare -f post >/dev/null 2>&1 || exit 10
post "$name"
"#;

/// Result of attempting an optional hook `uninstall(name)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstall {
    /// The hook file does not exist.
    MissingHook,
    /// The hook file exists but has no `uninstall` function.
    MissingFunction,
    /// The hook file or compatibility layer failed to source.
    SourceFailed,
    /// `uninstall(name)` exists but returned non-zero.
    Failed,
    /// `uninstall(name)` ran successfully.
    Removed,
}

/// Result of running `install(name)` for a custom dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// Hook file does not exist.
    MissingHook,
    /// Hook exists but cannot be used because a required function is missing.
    MissingFunction,
    /// Hook file or compatibility layer failed to source.
    SourceFailed,
    /// `exists(name)` reported the dependency was already installed.
    Already {
        /// Optional version output from `version(name)`.
        detail: String,
    },
    /// `install(name)` failed.
    Failed,
    /// `install(name)` ran successfully.
    Installed {
        /// Optional version output after install.
        detail: String,
    },
}

/// Result of running optional `post(name)` after a dependency changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Post {
    /// Hook file does not exist.
    MissingHook,
    /// Hook file exists but has no `post` function.
    MissingFunction,
    /// Hook file or compatibility layer failed to source.
    SourceFailed,
    /// `post(name)` returned non-zero.
    Failed,
    /// `post(name)` ran successfully.
    Ran,
}

/// Custom-status probe that evaluates hook `exists`/`version` in Bash.
#[derive(Debug, Clone)]
pub struct BashCustomProbe {
    shdeps_lib: PathBuf,
    inline_source: Option<&'static str>,
}

/// Per-update hook coordination marker directory.
#[derive(Debug, Clone)]
pub(crate) struct Txn {
    id: String,
    marker_dir: PathBuf,
}

impl Txn {
    /// Creates the marker directory used by hook subprocesses in one update.
    pub(crate) fn new(state_dir: &Path) -> Result<Self> {
        let id = txn_id();
        let marker_dir = state_dir.join(".changed-markers").join(&id);
        std::fs::create_dir_all(&marker_dir)?;
        Ok(Self { id, marker_dir })
    }

    /// Returns the opaque ID exported as `SHDEPS_UPDATE_TXN_ID`.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Collects and removes changed markers written by hook subprocesses.
    pub(crate) fn collect(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        collect_markers(&self.marker_dir, &self.marker_dir, &mut names)?;
        names.sort();
        names.dedup();
        Ok(names)
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        // Keep the directory alive throughout the whole update because the
        // prelude is intentionally tiny and may only `touch` marker files. A
        // best-effort drop cleanup avoids state-dir clutter without letting
        // cleanup failures affect the already-completed update result.
        let _ = std::fs::remove_dir_all(&self.marker_dir);
        if let Some(parent) = self.marker_dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

impl BashCustomProbe {
    /// Creates a probe using an explicit `shdeps.sh` compatibility layer.
    #[must_use]
    pub fn new(shdeps_lib: impl Into<PathBuf>) -> Self {
        Self {
            shdeps_lib: shdeps_lib.into(),
            inline_source: None,
        }
    }

    /// Creates a probe backed by the Rust-generated hook prelude.
    #[must_use]
    pub fn rust_prelude() -> Self {
        Self {
            shdeps_lib: PathBuf::from("__shdeps_inline_prelude__"),
            inline_source: Some(prelude::source()),
        }
    }

    /// Returns the configured compatibility-layer path.
    #[must_use]
    pub fn shdeps_lib(&self) -> &Path {
        &self.shdeps_lib
    }

    /// Runs the optional `uninstall(name)` hook for prune and method cleanup.
    pub fn uninstall(&self, name: &str, roots: &Roots) -> Result<Uninstall> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Uninstall::MissingHook);
        }
        if !self.available() {
            return Ok(Uninstall::SourceFailed);
        }

        let mut command = self.command(UNINSTALL_SCRIPT, name, &hook);
        apply_hook_env(&mut command, roots, name, "uninstall", None);
        let output = run_hook_command(command)?;

        Ok(match output.status.code() {
            Some(0) => Uninstall::Removed,
            Some(10) => Uninstall::MissingFunction,
            Some(11 | 12) => Uninstall::SourceFailed,
            _ => Uninstall::Failed,
        })
    }

    /// Runs `install(name)` for a custom dependency hook.
    pub fn install(&self, name: &str, roots: &Roots, reinstall: bool) -> Result<Install> {
        self.install_with_txn(name, roots, reinstall, None)
    }

    /// Runs `install(name)` with update-transaction context.
    pub(crate) fn install_with_txn(
        &self,
        name: &str,
        roots: &Roots,
        reinstall: bool,
        txn: Option<&Txn>,
    ) -> Result<Install> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Install::MissingHook);
        }
        if !self.available() {
            return Ok(Install::SourceFailed);
        }

        let mut command = self.command(INSTALL_SCRIPT, name, &hook);
        command.arg(if reinstall { "1" } else { "0" });
        apply_hook_env(&mut command, roots, name, "install", txn);
        let output = run_hook_command(command)?;

        let detail = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(match output.status.code() {
            Some(0) => Install::Installed { detail },
            Some(20) => Install::Already { detail },
            Some(10 | 13) => Install::MissingFunction,
            Some(11 | 12) => Install::SourceFailed,
            _ => Install::Failed,
        })
    }

    /// Runs optional `post(name)` for a dependency that changed.
    pub fn post(&self, name: &str, roots: &Roots) -> Result<Post> {
        self.post_with_txn(name, roots, None)
    }

    /// Runs optional `post(name)` with update-transaction context.
    pub(crate) fn post_with_txn(
        &self,
        name: &str,
        roots: &Roots,
        txn: Option<&Txn>,
    ) -> Result<Post> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Post::MissingHook);
        }
        if !self.available() {
            return Ok(Post::SourceFailed);
        }

        let mut command = self.command(POST_SCRIPT, name, &hook);
        apply_hook_env(&mut command, roots, name, "post", txn);
        let output = run_hook_command(command)?;

        Ok(match output.status.code() {
            Some(0) => Post::Ran,
            Some(10) => Post::MissingFunction,
            Some(11 | 12) => Post::SourceFailed,
            _ => Post::Failed,
        })
    }

    fn command(&self, script: &'static str, name: &str, hook: &Path) -> Command {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(script)
            .arg("shdeps-hook")
            .arg(name)
            .arg(&self.shdeps_lib)
            .arg(hook);
        if let Some(source) = self.inline_source {
            // The generated prelude is not a real file in dev/test builds. Pass
            // it through the environment so the tiny Bash runner can `eval`
            // the same source text without creating temp files that would need
            // cleanup or survive a crashed hook subprocess.
            command.env("SHDEPS_HOOK_PRELUDE_SOURCE", source);
        }
        command
    }

    fn available(&self) -> bool {
        self.inline_source.is_some() || self.shdeps_lib.is_file()
    }
}

fn apply_hook_env(
    command: &mut Command,
    roots: &Roots,
    name: &str,
    phase: &str,
    txn: Option<&Txn>,
) {
    command
        .env("SHDEPS_CONF_DIR", &roots.conf_dir)
        .env("SHDEPS_HOOKS_DIR", &roots.hooks_dir)
        .env("SHDEPS_STATE_DIR", &roots.state_dir)
        .env("SHDEPS_GIT_DEV_DIR", &roots.git_dev_dir)
        .env("SHDEPS_INSTALL_DIR", &roots.install_dir)
        .env("SHDEPS_BIN_DIR", &roots.bin_dir)
        .env("SHDEPS_CURRENT_DEP", name)
        .env("SHDEPS_HOOK_PHASE", phase)
        // Signal to any recursive `shdeps` invocation from inside the
        // hook that our parent already holds the per-state-dir flock.
        // The value is our own PID; the inner `StateLock::acquire`
        // accepts the reentry signal only when the env value equals
        // the child's `getppid()`. That binding blocks the "user
        // exported SHDEPS_STATE_LOCK_HELD in their shell" bypass —
        // see `state::REENTRY_ENV` for the full rationale.
        .env(crate::state::REENTRY_ENV, std::process::id().to_string());
    if let Some(txn) = txn {
        // This env var is the only parent/child coordination channel for
        // `shdeps_mark_changed`. Keeping it opaque prevents hooks from
        // depending on directory layout beyond the documented helper API.
        command.env("SHDEPS_UPDATE_TXN_ID", txn.id());
    }
}

fn collect_markers(root: &Path, dir: &Path, names: &mut Vec<String>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect_markers(root, &path, names)?;
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        if config::valid_dep_name(&name) {
            names.push(name);
        }
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

fn txn_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

impl CustomProbe for BashCustomProbe {
    fn installed_detail(&self, entry: &Entry, roots: &Roots) -> Result<Option<String>> {
        let hook = roots.hooks_dir.join(format!("{}.sh", entry.name));
        if !hook.is_file() || !self.available() {
            return Ok(None);
        }

        let mut command = self.command(STATUS_SCRIPT, &entry.name, &hook);
        // Status probes run `exists` and then optional `version` in one cheap
        // subprocess. Report the phase as `exists` because that is the required
        // predicate gate; hooks that need phase-specific install/post behavior
        // get separate subprocesses with more precise phases.
        apply_hook_env(&mut command, roots, &entry.name, "exists", None);
        let output = run_hook_command(command)?;

        if !output.status.success() {
            return Ok(None);
        }

        let detail = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        if detail.is_empty() {
            Ok(Some(String::new()))
        } else {
            Ok(Some(detail))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{BashCustomProbe, Install, Post, Txn, Uninstall, read_capped};
    use crate::config::parse_entry;
    use crate::runtime::Roots;
    use crate::status::CustomProbe;

    #[test]
    fn read_capped_keeps_only_first_max_bytes_but_drains_to_eof() {
        // The cap is the in-memory storage limit, not a read limit. The
        // reader must keep draining the source so a real hook child does
        // not block on a full pipe buffer; only the first `max_bytes`
        // are retained for the caller.
        let payload = vec![b'a'; 64 * 1024];
        let kept = read_capped(std::io::Cursor::new(payload.clone()), 16 * 1024);
        assert_eq!(kept.len(), 16 * 1024);
        assert!(kept.iter().all(|byte| *byte == b'a'));

        // A reader whose total bytes fit under the cap is returned in full.
        let small = b"hello world".to_vec();
        let kept = read_capped(std::io::Cursor::new(small.clone()), 16 * 1024);
        assert_eq!(kept, small);

        // A zero cap discards every byte but still returns Ok with empty.
        let kept = read_capped(std::io::Cursor::new(payload), 0);
        assert!(kept.is_empty());
    }

    #[test]
    fn txn_collects_valid_markers_and_ignores_invalid_names() {
        let roots = roots();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let txn = Txn::new(&roots.state_dir).unwrap();
        let marker_dir = txn.marker_dir.clone();
        fs::create_dir_all(marker_dir.join("owner")).unwrap();
        fs::write(marker_dir.join("owner/tool"), "").unwrap();
        fs::write(marker_dir.join("bad name"), "").unwrap();

        assert_eq!(txn.collect().unwrap(), vec!["owner/tool".to_owned()]);
        assert!(marker_dir.exists());

        drop(txn);
        assert!(!marker_dir.exists());
    }

    #[test]
    fn custom_probe_returns_version_when_exists_succeeds() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ "$1" == tool ]]; }
version() { printf '1.2.3\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let detail = probe
            .installed_detail(&parse_entry("tool|custom|-|-|-", None), &roots)
            .unwrap();

        assert_eq!(detail.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn custom_probe_treats_missing_hook_source_or_exists_as_missing() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("missing.sh"),
            "exists() { return 1; }\n",
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe
                .installed_detail(&parse_entry("missing|custom|-|-|-", None), &roots)
                .unwrap(),
            None
        );
        assert_eq!(
            probe
                .installed_detail(&parse_entry("no-hook|custom|-|-|-", None), &roots)
                .unwrap(),
            None
        );
    }

    #[test]
    fn custom_probe_sets_shdeps_root_environment_for_hooks() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("envtool.sh"),
            r#"
exists() {
  [[ "$SHDEPS_INSTALL_DIR" == */share ]] || return 1
  [[ "$SHDEPS_CURRENT_DEP" == envtool ]] || return 1
  [[ "$SHDEPS_HOOK_PHASE" == exists ]] || return 1
}
version() { printf '%s\n' "$SHDEPS_BIN_DIR"; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let detail = probe
            .installed_detail(&parse_entry("envtool|custom|-|-|-", None), &roots)
            .unwrap();

        assert_eq!(detail.as_deref(), Some(roots.bin_dir.to_str().unwrap()));
    }

    #[test]
    fn uninstall_runs_optional_hook_with_context() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
uninstall() {
  [[ "$SHDEPS_CURRENT_DEP" == tool ]] || return 1
  [[ "$SHDEPS_HOOK_PHASE" == uninstall ]] || return 1
  printf '%s\n' "$1" > "$SHDEPS_STATE_DIR/uninstalled"
}
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.uninstall("tool", &roots).unwrap();

        assert_eq!(result, Uninstall::Removed);
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("uninstalled")).unwrap(),
            "tool\n"
        );
    }

    #[test]
    fn install_runs_custom_hook_and_reports_change() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/tool-installed" ]]; }
install() { printf '1\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
version() { printf '2.0.0\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.install("tool", &roots, false).unwrap();

        assert_eq!(
            result,
            Install::Installed {
                detail: "2.0.0".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("tool-installed")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn install_skips_existing_custom_hook_unless_reinstalling() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        fs::write(roots.state_dir.join("tool-installed"), "old\n").unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { [[ -f "$SHDEPS_STATE_DIR/tool-installed" ]]; }
install() { printf 'new\n' > "$SHDEPS_STATE_DIR/tool-installed"; }
version() { printf 'old-version\n'; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe.install("tool", &roots, false).unwrap(),
            Install::Already {
                detail: "old-version".to_owned()
            }
        );
        assert_eq!(
            probe.install("tool", &roots, true).unwrap(),
            Install::Installed {
                detail: "old-version".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("tool-installed")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn post_runs_optional_hook_with_context() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
post() { printf '%s:%s\n' "$1" "$SHDEPS_HOOK_PHASE" > "$SHDEPS_STATE_DIR/post-ran"; }
"#,
        );

        let probe = BashCustomProbe::new(&lib);
        let result = probe.post("tool", &roots).unwrap();

        assert_eq!(result, Post::Ran);
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("post-ran")).unwrap(),
            "tool:post\n"
        );
    }

    #[test]
    fn uninstall_classifies_missing_and_failed_hooks() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(&roots.hooks_dir.join("no-fn.sh"), "exists() { :; }\n");
        write_hook(
            &roots.hooks_dir.join("fails.sh"),
            "uninstall() { return 7; }\n",
        );

        let probe = BashCustomProbe::new(&lib);

        assert_eq!(
            probe.uninstall("missing", &roots).unwrap(),
            Uninstall::MissingHook
        );
        assert_eq!(
            probe.uninstall("no-fn", &roots).unwrap(),
            Uninstall::MissingFunction
        );
        assert_eq!(probe.uninstall("fails", &roots).unwrap(), Uninstall::Failed);
        assert_eq!(
            BashCustomProbe::new(roots.home.join("missing-shdeps.sh"))
                .uninstall("fails", &roots)
                .unwrap(),
            Uninstall::SourceFailed
        );
    }

    fn write_hook(path: &PathBuf, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn roots() -> Roots {
        let root = temp_dir("hooks");
        Roots {
            conf_dir: root.join("config"),
            hooks_dir: root.join("config/hooks.d"),
            state_dir: root.join("state"),
            git_dev_dir: root.join("git"),
            install_dir: root.join("share"),
            bin_dir: root.join("bin"),
            home: root,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("shdeps-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
