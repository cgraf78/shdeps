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

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::cancellation::{self, Isolation};
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
/// Buffering the full child output would let a hook that emits unbounded text
/// (e.g., `version() { cat /var/log/syslog; }`
/// — a real footgun in the wild) could OOM the parent. We continue
/// draining the pipes after the cap so the child does not block on a
/// full pipe buffer, but only the first `MAX` bytes are kept in memory.
const DEFAULT_HOOK_MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Poll interval used while waiting for a hook child to exit.
///
/// Short hooks dominate warm updates, so 10 ms avoids adding one coarse wait
/// per custom dependency. This matches the general timed subprocess runner;
/// even a five-minute install performs only 30,000 nonblocking wait checks.
const HOOK_WAIT_POLL: Duration = Duration::from_millis(10);
const HOOK_WARNING_PREFIX: &str = "shdeps-hook-warning: ";
const HOOK_WARNING_MAX_BYTES: usize = 256;
const MAX_HOOK_FINGERPRINT_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const SUDO_REQUEST_EXIT_CODE: i32 = 75;
const SUDO_REQUEST_ENV: &str = "SHDEPS_HOOK_SUDO_REQUEST";
const SUDO_REQUEST_DIR: &str = ".hook-sudo-requests";
const SUDO_REQUEST_TOKEN: &[u8] = b"shdeps-hook-sudo-v1\n";
static SUDO_REQUEST_NONCE: AtomicU64 = AtomicU64::new(0);

struct HookOutput {
    output: std::process::Output,
    sudo_requested: bool,
}

#[derive(Clone, Copy)]
enum HookIsolation {
    DetachedSession,
    ParentSession,
}

struct SudoRequest {
    path: PathBuf,
    file: File,
}

impl SudoRequest {
    fn new(state_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let dir = state_dir.join(SUDO_REQUEST_DIR);
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&dir)?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::other(format!(
                "hook sudo request path is not a directory: {}",
                dir.display()
            )));
        }
        #[cfg(unix)]
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!(
            "{}-{}",
            txn_id(),
            SUDO_REQUEST_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        Ok(Self { path, file })
    }

    fn apply(&self, command: &mut Command) {
        command.env(SUDO_REQUEST_ENV, &self.path);
    }

    fn requested(&mut self) -> bool {
        if self.file.seek(SeekFrom::Start(0)).is_err() {
            return false;
        }
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes).is_ok() && bytes == SUDO_REQUEST_TOKEN
    }
}

impl Drop for SudoRequest {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Runs a configured hook command with a wall-clock timeout and a
/// per-stream output cap.
fn run_hook_command(
    mut command: Command,
    isolation: HookIsolation,
) -> io::Result<std::process::Output> {
    let timeout = hook_timeout();
    let max_bytes = hook_max_bytes();

    // The initial attempt starts a new session so it cannot prompt through the
    // parent's controlling terminal. An authenticated retry remains in the
    // parent session because sudo may scope its timestamp there, but receives
    // a dedicated process group so Shdeps still owns its cleanup boundary.
    let isolation = match isolation {
        HookIsolation::DetachedSession => Isolation::DetachedSession,
        HookIsolation::ParentSession => Isolation::ParentSession,
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cancellation::spawn_owned(&mut command, isolation, true)?;

    // Drain the pipes concurrently in background threads so the child
    // cannot wedge on a full pipe buffer while we wait for it to exit.
    // Each reader stops storing bytes after `max_bytes` but keeps
    // reading-and-discarding until EOF; without that, a runaway hook
    // would block forever on the next pipe write.
    let stdout = child.take_stdout().expect("stdout piped");
    let stderr = child.take_stderr().expect("stderr piped");
    let (stdout_handle, stderr_handle) = cancellation::spawn_output_readers(
        &mut child,
        Box::new(move || read_capped(stdout, max_bytes)),
        Box::new(move || read_capped(stderr, max_bytes)),
    )?;

    // The deadline covers both the hook leader and inherited output pipes.
    // A shell can exit while a background child keeps those pipes open; keep
    // polling instead of joining the readers immediately so that descendant
    // is still terminated at the configured deadline.
    let deadline = Instant::now() + timeout;
    let mut leader_exited = false;
    let (status, interrupted, stopped_boundary) = loop {
        if cancellation::received_signal().is_some() {
            break (child.stop(cancellation::TERMINATE_SIGNAL)?, true, true);
        }
        let output_drained = stdout_handle.is_finished() && stderr_handle.is_finished();
        if output_drained {
            if let Some(status) = child.wait_if_exited_and_output_drained()? {
                break (status, false, false);
            }
        } else if !leader_exited {
            leader_exited = child.exited()?;
        }
        if Instant::now() >= deadline {
            // Hook timed out. Signal the whole process group so any
            // grandchildren the hook backgrounded are reaped too —
            // otherwise they keep the inherited pipes open and the
            // reader threads hang past the timeout.
            break (child.stop(cancellation::KILL_SIGNAL)?, false, true);
        }
        thread::sleep(HOOK_WAIT_POLL);
    };

    if stopped_boundary {
        let drain_deadline = Instant::now() + Duration::from_millis(250);
        while !(stdout_handle.is_finished() && stderr_handle.is_finished())
            && Instant::now() < drain_deadline
        {
            thread::sleep(HOOK_WAIT_POLL);
        }
    }
    let stdout = if stdout_handle.is_finished() {
        cancellation::join_output_reader(stdout_handle, "hook stdout")
    } else {
        cancellation::unfinished_output_reader("hook stdout")
    };
    let stderr = if stderr_handle.is_finished() {
        cancellation::join_output_reader(stderr_handle, "hook stderr")
    } else {
        cancellation::unfinished_output_reader("hook stderr")
    };
    let stdout = stdout?;
    let stderr = stderr?;
    if interrupted || cancellation::received_signal().is_some() {
        return Err(io::Error::other("interrupted by signal"));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn run_mutating_hook_command(
    mut command: Command,
    state_dir: &Path,
    isolation: HookIsolation,
) -> io::Result<HookOutput> {
    let mut request = SudoRequest::new(state_dir)?;
    request.apply(&mut command);
    let output = run_hook_command(command, isolation)?;
    let sudo_requested =
        output.status.code() == Some(SUDO_REQUEST_EXIT_CODE) && request.requested();
    Ok(HookOutput {
        output,
        sudo_requested,
    })
}

/// Signals that a detached mutating hook needs its parent to authenticate sudo.
pub(crate) fn signal_parent_sudo_request() -> Result<bool> {
    let Some(request_path) = sudo_request_path() else {
        return Ok(false);
    };
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut request = options.open(request_path)?;
    if !request.metadata()?.file_type().is_file() {
        return Ok(false);
    }
    request.write_all(SUDO_REQUEST_TOKEN)?;
    Ok(true)
}

/// Returns whether this process has a syntactically valid parent-sudo channel.
pub(crate) fn parent_sudo_request_configured() -> bool {
    sudo_request_path().is_some()
}

fn sudo_request_path() -> Option<PathBuf> {
    let phase = std::env::var("SHDEPS_HOOK_PHASE").unwrap_or_default();
    if !matches!(phase.as_str(), "install" | "post" | "uninstall") {
        return None;
    }
    let request_path = std::env::var_os(SUDO_REQUEST_ENV).map(PathBuf::from)?;
    let state_dir = std::env::var_os("SHDEPS_STATE_DIR").map(PathBuf::from)?;
    if request_path.parent() != Some(state_dir.join(SUDO_REQUEST_DIR).as_path()) {
        return None;
    }
    Some(request_path)
}

fn read_capped<R: Read>(mut reader: R, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => return Ok(kept),
            Ok(n) => n,
            Err(error) => return Err(error),
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
// Overwriting an inherited value here is hygiene for legitimate reentry, not a
// trust boundary. It keeps a stale or incidental value from breaking the hook
// chain, but a caller can deliberately forge the cooperative parent-PID signal.
// `StateLock` documents why that trade-off is acceptable for a same-user
// deadlock-prevention mechanism.
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
if declare -f shdeps_mark_changed >/dev/null 2>&1; then
  shdeps_mark_changed "$name" || exit 15
fi
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
    /// The detached hook needs its attached parent to authenticate sudo.
    SudoRequired,
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
    /// The detached hook needs its attached parent to authenticate sudo.
    SudoRequired,
    /// `exists(name)` reported the dependency was already installed.
    Already {
        /// Optional version output from `version(name)`.
        detail: String,
    },
    /// `install(name)` failed, optionally with a sanitized hook-authored warning.
    Failed {
        /// Safe phase context emitted through `shdeps_warn`.
        detail: String,
    },
    /// `install(name)` ran successfully.
    Installed {
        /// Optional version output after install.
        detail: String,
    },
}

/// Result of running optional `post(name)` after a dependency changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Post {
    /// Hook file does not exist.
    MissingHook,
    /// Hook file exists but has no `post` function.
    MissingFunction,
    /// Hook file or compatibility layer failed to source.
    SourceFailed,
    /// The detached hook needs its attached parent to authenticate sudo.
    SudoRequired,
    /// `post(name)` declined with exit code 2 (hook-declined/mismatch
    /// convention). The hook chose not to run, so this is not a failure.
    Skipped,
    /// `post(name)` failed, optionally with a sanitized hook-authored warning.
    Failed {
        /// Safe phase context emitted through `shdeps_warn`.
        detail: String,
    },
    /// `post(name)` ran successfully.
    Ran,
}

/// Custom-status probe that evaluates hook `exists`/`version` in Bash.
#[derive(Debug, Clone)]
pub struct BashCustomProbe {
    shdeps_lib: PathBuf,
    inline_source: Option<&'static str>,
    package_manager: Option<String>,
    quiet: Option<bool>,
    platform: Option<String>,
}

/// Per-update hook coordination marker directory.
#[derive(Debug, Clone)]
pub(crate) struct Txn {
    id: String,
    marker_dir: PathBuf,
    state_dir: PathBuf,
}

/// Durable pre-mutation post-hook obligation.
///
/// `begin` must complete before the irreversible operation starts. Dropping an
/// unresolved intent deliberately leaves the marker in place: interruption or
/// an ambiguous installer failure may have committed a side effect. Only a
/// caller that proved no change may remove a marker created by this attempt.
pub(crate) struct MutationIntent {
    state_dir: PathBuf,
    name: String,
    started: bool,
    preexisting: bool,
}

impl Txn {
    /// Creates the marker directory used by hook subprocesses in one update.
    pub(crate) fn new(state_dir: &Path) -> Result<Self> {
        promote_abandoned_markers(state_dir)?;
        let id = txn_id();
        let marker_dir = state_dir.join(".changed-markers").join(&id);
        std::fs::create_dir_all(&marker_dir)?;
        Ok(Self {
            id,
            marker_dir,
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// Returns the opaque ID exported as `SHDEPS_UPDATE_TXN_ID`.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Prepares the exact parent directory needed by the shell-local marker.
    pub(crate) fn prepare_marker(&self, name: &str) -> Result<()> {
        if !config::valid_dep_name(name) {
            return Ok(());
        }
        let marker = self.marker_dir.join(name);
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Collects changed markers written by hook subprocesses.
    pub(crate) fn collect(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        collect_markers(&self.marker_dir, &self.marker_dir, &mut names)?;
        names.sort();
        names.dedup();
        for name in &names {
            mark_pending_post(&self.state_dir, name)?;
            // Promotion is deliberately ordered durable-first: a crash before
            // this unlink leaves both copies and safely retries promotion,
            // while a crash afterward still leaves the durable obligation.
            // Consuming the ephemeral marker here prevents `Drop::drop` from
            // recreating an obligation that a successful post already
            // acknowledged later in this same update.
            std::fs::remove_file(self.marker_dir.join(name))?;
        }
        Ok(names)
    }

    /// Persists one post-hook obligation before cancellation can unwind.
    pub(crate) fn mark_pending(&self, name: &str) -> Result<()> {
        mark_pending_post(&self.state_dir, name)
    }

    /// Creates a lazy intent for one dependency mutation.
    pub(crate) fn mutation(&self, name: &str) -> MutationIntent {
        MutationIntent::new(&self.state_dir, name)
    }

    /// Returns all post-hook obligations retained from this or an earlier run.
    pub(crate) fn pending(&self) -> Result<Vec<String>> {
        pending_posts(&self.state_dir)
    }

    /// Reports whether one dependency still owes its post hook.
    pub(crate) fn has_pending(&self, name: &str) -> Result<bool> {
        Ok(self.pending()?.iter().any(|pending| pending == name))
    }

    /// Acknowledges one obligation after its post classification is complete.
    pub(crate) fn acknowledge(&self, name: &str) -> Result<()> {
        acknowledge_pending_post(&self.state_dir, name)
    }
}

impl MutationIntent {
    pub(crate) fn new(state_dir: &Path, name: &str) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            name: name.to_owned(),
            started: false,
            preexisting: false,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Persists the post obligation before the associated side effect begins.
    pub(crate) fn begin(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        if !config::valid_dep_name(&self.name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid dependency name for mutation intent",
            )
            .into());
        }
        self.preexisting =
            std::fs::symlink_metadata(self.state_dir.join(PENDING_POSTS_DIR).join(&self.name))
                .is_ok();
        mark_pending_post(&self.state_dir, &self.name)?;
        self.started = true;
        Ok(())
    }

    /// Resolves an attempted mutation and reports whether a post remains owed.
    pub(crate) fn resolve(&mut self, changed: bool) -> Result<bool> {
        if !self.started {
            return Ok(self.preexisting);
        }
        let pending = self.preexisting || changed;
        if !pending {
            acknowledge_pending_post(&self.state_dir, &self.name)?;
        }
        self.started = false;
        Ok(pending)
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        // A hook may finish its durable mutation and marker just before the
        // parent observes cancellation. Promote every marker before deleting
        // this ephemeral transaction so the next update still runs the owed
        // post hook. If promotion fails, retain the transaction directory;
        // `Txn::new` will retry it under the next state-lock owner.
        if self.collect().is_err() {
            return;
        }
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
            package_manager: None,
            quiet: None,
            platform: None,
        }
    }

    /// Creates a probe backed by the Rust-generated hook prelude.
    #[must_use]
    pub fn rust_prelude() -> Self {
        Self {
            shdeps_lib: PathBuf::from("__shdeps_inline_prelude__"),
            inline_source: Some(prelude::source()),
            package_manager: None,
            quiet: None,
            platform: None,
        }
    }

    /// Uses the package manager already detected by the parent command.
    #[must_use]
    pub fn with_package_manager(mut self, package_manager: impl Into<String>) -> Self {
        self.package_manager = Some(package_manager.into());
        self
    }

    /// Uses the parent command's parsed quiet policy for hook subprocesses.
    #[must_use]
    pub fn with_quiet(mut self, quiet: bool) -> Self {
        self.quiet = Some(quiet);
        self
    }

    /// Uses the parent command's already-resolved platform for hook subprocesses.
    ///
    /// The hook prelude answers `shdeps_platform` from this export instead of
    /// spawning a recursive `__api` call. Probes without a platform keep the
    /// bridge fallback for compatibility.
    #[must_use]
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    /// Returns the configured compatibility-layer path.
    #[must_use]
    pub fn shdeps_lib(&self) -> &Path {
        &self.shdeps_lib
    }

    /// Returns a stable digest of every file-backed input that selects custom
    /// install code. Recovery uses it only to decide whether an interrupted
    /// install may be retried; it never treats a matching digest as proof that
    /// the earlier arbitrary side effect completed.
    pub(crate) fn install_fingerprint(&self, name: &str, roots: &Roots) -> Result<Option<String>> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        let Some(hook_bytes) = read_regular_fingerprint_input(&hook)? else {
            return Ok(None);
        };
        let library = if let Some(source) = self.inline_source {
            source.as_bytes().to_vec()
        } else {
            let Some(bytes) = read_regular_fingerprint_input(&self.shdeps_lib)? else {
                return Ok(None);
            };
            bytes
        };
        let mut input = Vec::with_capacity(hook_bytes.len() + library.len() + 128);
        for bytes in [
            hook_bytes.as_slice(),
            library.as_slice(),
            self.package_manager.as_deref().unwrap_or("").as_bytes(),
            if self.quiet == Some(true) { b"1" } else { b"0" },
        ] {
            input.extend_from_slice(&bytes.len().to_le_bytes());
            input.extend_from_slice(bytes);
        }
        Ok(Some(crate::checksum::sha256_hex(&input)))
    }

    /// Runs the optional `uninstall(name)` hook for prune and method cleanup.
    pub fn uninstall(&self, name: &str, roots: &Roots) -> Result<Uninstall> {
        self.uninstall_with_isolation(name, roots, HookIsolation::DetachedSession)
    }

    /// Retries `uninstall(name)` after the parent authenticated sudo.
    pub(crate) fn retry_uninstall(&self, name: &str, roots: &Roots) -> Result<Uninstall> {
        self.uninstall_with_isolation(name, roots, HookIsolation::ParentSession)
    }

    fn uninstall_with_isolation(
        &self,
        name: &str,
        roots: &Roots,
        isolation: HookIsolation,
    ) -> Result<Uninstall> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Uninstall::MissingHook);
        }
        if !self.available() {
            return Ok(Uninstall::SourceFailed);
        }

        let mut command = self.command(UNINSTALL_SCRIPT, name, &hook);
        apply_hook_env(&mut command, roots, name, "uninstall", self, None);
        let output = run_mutating_hook_command(command, &roots.state_dir, isolation)?;

        if output.sudo_requested {
            return Ok(Uninstall::SudoRequired);
        }

        Ok(match output.output.status.code() {
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
        self.install_with_txn_isolation(name, roots, reinstall, txn, HookIsolation::DetachedSession)
    }

    /// Retries `install(name)` after the parent authenticated sudo.
    pub(crate) fn retry_install_with_txn(
        &self,
        name: &str,
        roots: &Roots,
        reinstall: bool,
        txn: Option<&Txn>,
    ) -> Result<Install> {
        self.install_with_txn_isolation(name, roots, reinstall, txn, HookIsolation::ParentSession)
    }

    fn install_with_txn_isolation(
        &self,
        name: &str,
        roots: &Roots,
        reinstall: bool,
        txn: Option<&Txn>,
        isolation: HookIsolation,
    ) -> Result<Install> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Install::MissingHook);
        }
        if !self.available() {
            return Ok(Install::SourceFailed);
        }
        if let Some(txn) = txn {
            txn.prepare_marker(name)?;
        }

        let mut command = self.command(INSTALL_SCRIPT, name, &hook);
        command.arg(if reinstall { "1" } else { "0" });
        apply_hook_env(&mut command, roots, name, "install", self, txn);
        let output = run_mutating_hook_command(command, &roots.state_dir, isolation)?;

        if output.sudo_requested {
            return Ok(Install::SudoRequired);
        }

        let detail = String::from_utf8_lossy(&output.output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        Ok(match output.output.status.code() {
            Some(0) => Install::Installed { detail },
            Some(20) => Install::Already { detail },
            Some(10 | 13) => Install::MissingFunction,
            Some(11 | 12) => Install::SourceFailed,
            _ => Install::Failed {
                detail: failed_hook_detail(&output.output.stderr),
            },
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
        self.post_with_txn_isolation(name, roots, txn, HookIsolation::DetachedSession)
    }

    /// Retries `post(name)` after the parent authenticated sudo.
    pub(crate) fn retry_post_with_txn(
        &self,
        name: &str,
        roots: &Roots,
        txn: Option<&Txn>,
    ) -> Result<Post> {
        self.post_with_txn_isolation(name, roots, txn, HookIsolation::ParentSession)
    }

    fn post_with_txn_isolation(
        &self,
        name: &str,
        roots: &Roots,
        txn: Option<&Txn>,
        isolation: HookIsolation,
    ) -> Result<Post> {
        let hook = roots.hooks_dir.join(format!("{name}.sh"));
        if !hook.is_file() {
            return Ok(Post::MissingHook);
        }
        if !self.available() {
            return Ok(Post::SourceFailed);
        }

        let mut command = self.command(POST_SCRIPT, name, &hook);
        apply_hook_env(&mut command, roots, name, "post", self, txn);
        let output = run_mutating_hook_command(command, &roots.state_dir, isolation)?;

        if output.sudo_requested {
            return Ok(Post::SudoRequired);
        }

        Ok(match output.output.status.code() {
            Some(0) => Post::Ran,
            Some(2) => Post::Skipped,
            Some(10) => Post::MissingFunction,
            Some(11 | 12) => Post::SourceFailed,
            _ => Post::Failed {
                detail: failed_hook_detail(&output.output.stderr),
            },
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

fn read_regular_fingerprint_input(path: &Path) -> Result<Option<Vec<u8>>> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let expected = std::fs::metadata(&canonical)?;
    if !expected.is_file() || expected.len() > MAX_HOOK_FINGERPRINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "hook input is not a bounded regular file: {}",
                path.display()
            ),
        )
        .into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(&canonical)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() != expected.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hook input changed while opening: {}", path.display()),
        )
        .into());
    }
    #[cfg(unix)]
    if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "hook input identity changed while opening: {}",
                path.display()
            ),
        )
        .into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_HOOK_FINGERPRINT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).ok() != Some(expected.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hook input changed while reading: {}", path.display()),
        )
        .into());
    }
    Ok(Some(bytes))
}

fn failed_hook_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .filter_map(|line| line.strip_prefix(HOOK_WARNING_PREFIX))
        .map(str::trim)
        .find(|detail| safe_hook_detail(detail))
        .unwrap_or_default()
        .to_owned()
}

fn safe_hook_detail(detail: &str) -> bool {
    if detail.is_empty() || detail.len() > HOOK_WARNING_MAX_BYTES {
        return false;
    }
    let lowered = detail.to_ascii_lowercase();
    if [
        "://",
        "authorization",
        "bearer",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
    {
        return false;
    }
    detail.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || matches!(character, ' ' | '-' | '_' | '.' | ':' | ',' | '(' | ')')
    })
}

fn apply_hook_env(
    command: &mut Command,
    roots: &Roots,
    name: &str,
    phase: &str,
    probe: &BashCustomProbe,
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
    if let Some(package_manager) = probe.package_manager.as_deref() {
        // Hooks must use the same manager the parent command already
        // detected. Setting even an empty value explicitly prevents an
        // inherited shell cache from spoofing a different runtime through
        // `shdeps_pkg_mgr`. Direct library callers that did not supply a
        // detected manager retain the inherited environment for compatibility.
        command.env("SHDEPS_PKG_MGR", package_manager);
    }
    if let Some(quiet) = probe.quiet {
        command.env("SHDEPS_QUIET", if quiet { "1" } else { "0" });
    }
    if let Some(platform) = probe.platform.as_deref() {
        // The name is prelude-read only: the Rust runtime never consults
        // it, so exporting the resolved value cannot leak parent CLI flags
        // into nested invocations the way reusing `SHDEPS_FORCE` would.
        command.env("SHDEPS_HOOK_PLATFORM", platform);
    }
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
    }
    Ok(())
}

const PENDING_POSTS_DIR: &str = ".pending-posts";

/// Serializes pending-post marker updates across parallel tool threads.
///
/// `mark` creates parent dirs while `acknowledge` removes them; without
/// mutual exclusion the mkdir and the climbing rmdir interleave and the
/// write fails with ENOENT. Cross-process updates serialize on the state
/// lock, so a process-local mutex is complete (the atomic writer's ENOENT
/// retry remains as a net for reentrant nested updates).
static PENDING_POSTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn mark_pending_post(state_dir: &Path, name: &str) -> Result<()> {
    if !config::valid_dep_name(name) {
        return Ok(());
    }
    let _guard = PENDING_POSTS_LOCK.lock().unwrap();
    crate::state::write_atomic(&state_dir.join(PENDING_POSTS_DIR).join(name), "pending\n")
}

fn pending_posts(state_dir: &Path) -> Result<Vec<String>> {
    let root = state_dir.join(PENDING_POSTS_DIR);
    let mut names = Vec::new();
    collect_markers(&root, &root, &mut names)?;
    names.sort();
    names.dedup();
    Ok(names)
}

fn acknowledge_pending_post(state_dir: &Path, name: &str) -> Result<()> {
    if !config::valid_dep_name(name) {
        return Ok(());
    }
    let _guard = PENDING_POSTS_LOCK.lock().unwrap();
    let root = state_dir.join(PENDING_POSTS_DIR);
    let marker = root.join(name);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let mut parent = marker.parent();
    while let Some(directory) = parent.filter(|directory| *directory != root) {
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
    let _ = std::fs::remove_dir(root);
    Ok(())
}

fn promote_abandoned_markers(state_dir: &Path) -> Result<()> {
    let root = state_dir.join(".changed-markers");
    let Ok(transactions) = std::fs::read_dir(&root) else {
        return Ok(());
    };
    for transaction in transactions {
        let path = transaction?.path();
        if !path.is_dir() {
            continue;
        }
        let mut names = Vec::new();
        collect_markers(&path, &path, &mut names)?;
        for name in names {
            mark_pending_post(state_dir, &name)?;
        }
        std::fs::remove_dir_all(path)?;
    }
    let _ = std::fs::remove_dir(root);
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
        apply_hook_env(&mut command, roots, &entry.name, "exists", self, None);
        let output = run_hook_command(command, HookIsolation::DetachedSession)?;

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
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::process::Command;

    use super::{
        BashCustomProbe, Install, Post, SUDO_REQUEST_TOKEN, SudoRequest, Txn, Uninstall,
        apply_hook_env, read_capped,
    };
    use crate::config::parse_entry;
    use crate::runtime::Roots;
    use crate::status::CustomProbe;

    struct FailingReader;

    impl std::io::Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected hook read failure"))
        }
    }

    #[test]
    fn read_capped_keeps_only_first_max_bytes_but_drains_to_eof() {
        // The cap is the in-memory storage limit, not a read limit. The
        // reader must keep draining the source so a real hook child does
        // not block on a full pipe buffer; only the first `max_bytes`
        // are retained for the caller.
        let payload = vec![b'a'; 64 * 1024];
        let kept = read_capped(std::io::Cursor::new(payload.clone()), 16 * 1024).unwrap();
        assert_eq!(kept.len(), 16 * 1024);
        assert!(kept.iter().all(|byte| *byte == b'a'));

        // A reader whose total bytes fit under the cap is returned in full.
        let small = b"hello world".to_vec();
        let kept = read_capped(std::io::Cursor::new(small.clone()), 16 * 1024).unwrap();
        assert_eq!(kept, small);

        // A zero cap discards every byte but still returns Ok with empty.
        let kept = read_capped(std::io::Cursor::new(payload), 0).unwrap();
        assert!(kept.is_empty());
    }

    #[test]
    fn read_capped_preserves_reader_errors() {
        let error = read_capped(FailingReader, 16).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("injected hook read failure"));
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
        txn.acknowledge("owner/tool").unwrap();
        assert!(
            txn.collect().unwrap().is_empty(),
            "a marker promoted to durable storage must not be promoted again"
        );
        assert!(txn.pending().unwrap().is_empty());
        assert!(marker_dir.exists());

        drop(txn);
        assert!(!marker_dir.exists());
    }

    #[test]
    fn mutation_intent_is_durable_before_side_effect_and_clears_only_proven_no_change() {
        let roots = roots();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let txn = Txn::new(&roots.state_dir).unwrap();
        let mut intent = txn.mutation("owner/tool");

        intent.begin().unwrap();
        assert_eq!(txn.pending().unwrap(), vec!["owner/tool".to_owned()]);
        assert!(!intent.resolve(false).unwrap());
        assert!(txn.pending().unwrap().is_empty());

        let mut committed = txn.mutation("owner/tool");
        committed.begin().unwrap();
        assert!(committed.resolve(true).unwrap());
        assert_eq!(txn.pending().unwrap(), vec!["owner/tool".to_owned()]);
    }

    #[test]
    fn invalid_pending_post_storage_fails_before_mutation_can_start() {
        let roots = roots();
        fs::create_dir_all(&roots.state_dir).unwrap();
        fs::write(
            roots.state_dir.join(super::PENDING_POSTS_DIR),
            "not a directory\n",
        )
        .unwrap();
        let txn = Txn::new(&roots.state_dir).unwrap();
        let mut intent = txn.mutation("owner/tool");
        let mutated = std::cell::Cell::new(false);

        let result = intent.begin().map(|()| mutated.set(true));

        assert!(result.is_err());
        assert!(!mutated.get(), "side effect ran before durable intent");
    }

    #[test]
    fn sudo_request_is_private_and_bound_to_parent_created_inode() {
        let roots = roots();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let mut request = SudoRequest::new(&roots.state_dir).unwrap();
        let request_path = request.path.clone();
        let request_dir = request_path.parent().unwrap().to_path_buf();

        assert_eq!(
            fs::metadata(&request_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&request_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let replacement = roots.state_dir.join("replacement-request");
        fs::write(&replacement, SUDO_REQUEST_TOKEN).unwrap();
        fs::remove_file(&request_path).unwrap();
        symlink(&replacement, &request_path).unwrap();

        assert!(
            !request.requested(),
            "path replacement must not authenticate a request the parent did not receive"
        );
    }

    #[test]
    fn sudo_request_refuses_symlinked_control_directory() {
        let roots = roots();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let replacement = roots.state_dir.join("replacement-directory");
        fs::create_dir_all(&replacement).unwrap();
        symlink(&replacement, roots.state_dir.join(super::SUDO_REQUEST_DIR)).unwrap();

        let error = match SudoRequest::new(&roots.state_dir) {
            Ok(_) => panic!("symlinked request directory must be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("is not a directory"));
        assert!(fs::read_dir(&replacement).unwrap().next().is_none());
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
    fn hook_environment_exports_platform_for_prelude_cache() {
        let roots = roots();
        let probe = BashCustomProbe::new("shdeps.sh");
        let mut without = Command::new("true");
        apply_hook_env(&mut without, &roots, "tool", "exists", &probe, None);
        assert!(
            without
                .get_envs()
                .all(|(name, _)| name != "SHDEPS_HOOK_PLATFORM")
        );

        let probe = BashCustomProbe::new("shdeps.sh").with_platform("linux");
        let mut with = Command::new("true");
        apply_hook_env(&mut with, &roots, "tool", "exists", &probe, None);
        assert_eq!(
            with.get_envs()
                .find(|(name, _)| *name == "SHDEPS_HOOK_PLATFORM")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("linux"))
        );
    }

    #[test]
    fn hook_environment_only_overrides_package_manager_when_configured() {
        let roots = roots();
        let probe = BashCustomProbe::new("shdeps.sh");
        let mut inherited = Command::new("true");
        apply_hook_env(&mut inherited, &roots, "tool", "exists", &probe, None);
        assert!(
            inherited
                .get_envs()
                .all(|(name, _)| name != "SHDEPS_PKG_MGR")
        );

        let probe = BashCustomProbe::new("shdeps.sh").with_package_manager("");
        let mut detected_empty = Command::new("true");
        apply_hook_env(&mut detected_empty, &roots, "tool", "exists", &probe, None);
        assert_eq!(
            detected_empty
                .get_envs()
                .find(|(name, _)| *name == "SHDEPS_PKG_MGR")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new(""))
        );

        let probe = BashCustomProbe::new("shdeps.sh").with_package_manager("dnf");
        let mut detected_dnf = Command::new("true");
        apply_hook_env(&mut detected_dnf, &roots, "tool", "exists", &probe, None);
        assert_eq!(
            detected_dnf
                .get_envs()
                .find(|(name, _)| *name == "SHDEPS_PKG_MGR")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("dnf"))
        );
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
    fn rust_prelude_preserves_safe_install_failure_warning() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { return 1; }
install() {
  shdeps_warn 'google-java-format metadata download failed'
  return 42
}
"#,
        );

        let result = BashCustomProbe::rust_prelude()
            .install("tool", &roots, false)
            .unwrap();

        assert_eq!(
            result,
            Install::Failed {
                detail: "google-java-format metadata download failed".to_owned()
            }
        );
    }

    #[test]
    fn compatibility_layer_preserves_safe_install_failure_warning() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { return 1; }
install() {
  shdeps_warn 'php-cs-fixer asset download failed'
  return 42
}
"#,
        );

        let compatibility_layer = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shdeps.sh");
        let result = BashCustomProbe::new(compatibility_layer)
            .install("tool", &roots, false)
            .unwrap();

        if !compatibility_bash_supported() {
            assert_eq!(result, Install::SourceFailed);
            return;
        }

        assert_eq!(
            result,
            Install::Failed {
                detail: "php-cs-fixer asset download failed".to_owned()
            }
        );
    }

    #[test]
    fn compatibility_layer_exposes_bounded_curl_to_custom_hooks() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
exists() { return 1; }
curl() { printf '%s\n' "$@" > "$SHDEPS_STATE_DIR/curl-args"; }
install() {
  shdeps_curl -fsSL --no-netrc https://example.invalid/tool.tar.gz -o /dev/null
}
"#,
        );

        let compatibility_layer = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shdeps.sh");
        let result = BashCustomProbe::new(compatibility_layer)
            .install("tool", &roots, false)
            .unwrap();

        if !compatibility_bash_supported() {
            assert_eq!(result, Install::SourceFailed);
            return;
        }

        assert_eq!(
            result,
            Install::Installed {
                detail: String::new()
            }
        );
        assert_eq!(
            fs::read_to_string(roots.state_dir.join("curl-args")).unwrap(),
            concat!(
                "--connect-timeout\n10\n",
                "--speed-limit\n1024\n",
                "--speed-time\n60\n",
                "--retry\n3\n",
                "-fsSL\n--no-netrc\n",
                "https://example.invalid/tool.tar.gz\n",
                "-o\n/dev/null\n"
            )
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
    fn post_failure_carries_sanitized_stderr_detail() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        write_hook(
            &roots.hooks_dir.join("tool.sh"),
            r#"
post() {
  shdeps_warn 'link farm refresh failed'
  return 1
}
"#,
        );

        let result = BashCustomProbe::rust_prelude()
            .post("tool", &roots)
            .unwrap();

        assert_eq!(
            result,
            Post::Failed {
                detail: "link farm refresh failed".to_owned()
            }
        );
    }

    #[test]
    fn post_exit_code_two_is_skipped_not_failed() {
        let roots = roots();
        fs::create_dir_all(&roots.hooks_dir).unwrap();
        fs::create_dir_all(&roots.state_dir).unwrap();
        let lib = roots.home.join("shdeps.sh");
        fs::write(&lib, "shdeps_version() { :; }\n").unwrap();
        write_hook(&roots.hooks_dir.join("tool.sh"), "post() { return 2; }\n");

        let result = BashCustomProbe::new(&lib).post("tool", &roots).unwrap();

        assert_eq!(result, Post::Skipped);
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

    fn compatibility_bash_supported() -> bool {
        let mut command = Command::new("bash");
        command.args([
            "-c",
            "((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3)))",
        ]);
        crate::test_support::run_subprocess(command)
            .unwrap()
            .status
            .success()
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
        crate::test_support::temp_dir(&format!("shdeps-{name}"))
    }

    #[test]
    fn concurrent_pending_post_mark_and_acknowledge_never_loses_parent_dirs() {
        use super::{acknowledge_pending_post, mark_pending_post};

        let state_dir = temp_dir("pending-posts-race");
        let mut handles = Vec::new();
        for thread in 0..8 {
            let dir = state_dir.clone();
            handles.push(std::thread::spawn(move || {
                for item in 0..200 {
                    // Shared names empty the root often, forcing the
                    // acknowledge-climbs-while-another-marks interleave that
                    // parallel tools hit rarely with distinct names.
                    let name = format!("tool-{}", (thread + item) % 2);
                    mark_pending_post(&dir, &name).unwrap();
                    acknowledge_pending_post(&dir, &name).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
