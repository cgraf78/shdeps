//! Process-entry cancellation and owned subprocess teardown.
//!
//! The standalone CLI catches the conventional termination signals so it can
//! stop subprocess boundaries that it deliberately moved out of the caller's
//! process group or session. Signal handlers only publish the first signal;
//! normal Rust code performs TERM, bounded KILL escalation, output draining,
//! and reaping.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::collections::VecDeque;
#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::sync::Condvar;
#[cfg(unix)]
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::sync::{RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

const SIGNAL_CLOSED: i32 = -1;
// Preserve the timed-command runner's existing 250ms TERM grace. An outer
// supervisor can apply its own longer fallback, so nested escalation remains
// bounded without compounding interactive cancellation latency.
#[cfg(unix)]
const GRACE: Duration = Duration::from_millis(250);
// Killed descendants still need to be observed twice and adopted children
// reaped. Keep that verification bounded but give loaded/slow procfs systems
// enough room for two fresh scans; the user-visible TERM grace stays 250ms.
#[cfg(unix)]
const KILL_VERIFY_GRACE: Duration = Duration::from_secs(1);
// The discovery window can end immediately after delivering SIGKILL to a
// just-observed escapee. Reserve separate time for the kernel to retire that
// identity and for two subsequent stable-empty observations; otherwise a
// successful final delivery can be misreported as incomplete cleanup.
#[cfg(unix)]
const KILL_SETTLE_GRACE: Duration = Duration::from_millis(500);
#[cfg(unix)]
const POLL: Duration = Duration::from_millis(20);
#[cfg(unix)]
const TRACK_SNAPSHOT_BUDGET: Duration = Duration::from_millis(250);
#[cfg(unix)]
const LINUX_SNAPSHOT_TTL: Duration = Duration::from_millis(500);
#[cfg(unix)]
const LEADER_EXIT_SNAPSHOT_BUDGET: Duration = Duration::from_secs(1);
// A portable snapshot spawns `ps` plus per-PID probes; on loaded macOS
// runners a single whole-table scan can exceed 1s, which permanently
// fails an otherwise clean teardown (the discovery error is retained).
// 5s tolerates loaded scans while still bounding a genuinely wedged `ps`.
#[cfg(target_os = "macos")]
const CLEANUP_SNAPSHOT_BUDGET: Duration = Duration::from_secs(5);
#[cfg(all(unix, not(target_os = "macos")))]
const CLEANUP_SNAPSHOT_BUDGET: Duration = Duration::from_secs(1);
#[cfg(unix)]
const TRACK_POLL: Duration = Duration::from_millis(50);
// A concurrent exec can present a markerless row that gains its marker
// microseconds later, and a short-lived unrelated child can exit between
// back-to-back inspections. Re-verify once after this settle before the
// adoptee policy fails closed so one transient row cannot poison every
// concurrent boundary's completion proof.
#[cfg(any(target_os = "linux", target_os = "android"))]
const AMBIGUOUS_ADOPTEE_SETTLE: Duration = Duration::from_millis(50);
#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
const PORTABLE_SNAPSHOT_TTL: Duration = Duration::from_millis(500);
// Grace-loop polling on portable platforms shares whole-system scans at
// this cadence instead of spawning `ps` per 20ms poll. Concurrent
// boundaries single-flight through one scan per window, which collapses
// the fork storm that otherwise makes every scan slower (and can push a
// single scan past CLEANUP_SNAPSHOT_BUDGET on loaded macOS runners).
// 50ms matches the Linux fresh-discovery cadence (TRACK_POLL) and stays
// well under the 250ms grace, so genuine exits are still acknowledged
// within grace; see grace_empty_counts for why consecutive empties must
// be spaced by this TTL to remain independent evidence.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const GRACE_SNAPSHOT_TTL: Duration = Duration::from_millis(50);
const OUTPUT_POLL: Duration = Duration::from_millis(10);
static RECEIVED: AtomicI32 = AtomicI32::new(0);
static ACTIVE_HANDLERS: AtomicUsize = AtomicUsize::new(0);
static SIGNALS_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static TERMINAL_EXIT_OVERRIDE: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static TERMINAL_HANDOFF_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static LAUNCH_CANCEL_READER: AtomicI32 = AtomicI32::new(-1);
#[cfg(unix)]
static LAUNCH_CANCEL_WRITER: AtomicI32 = AtomicI32::new(-1);
static CLEANUP_FAILED: AtomicBool = AtomicBool::new(false);
static SIGNAL_OWNER: Mutex<()> = Mutex::new(());
#[cfg(unix)]
static TARGET_SIGCHLD: Mutex<Option<libc::sigaction>> = Mutex::new(None);
static CLEANUP_DIAGNOSTICS: Mutex<Vec<String>> = Mutex::new(Vec::new());
#[cfg(unix)]
static TERMINAL_OWNER: Mutex<()> = Mutex::new(());
static BOUNDARY_COUNTER: AtomicUsize = AtomicUsize::new(0);
static BOUNDARY_PROCESS_NONCE: OnceLock<Option<String>> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static EXACT_DESCENDANT_AUTHORITY: OnceLock<bool> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static PENDING_REAPS: OnceLock<Mutex<PendingReaps>> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static LINUX_SNAPSHOT_CACHE: OnceLock<SnapshotCache> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static ACTIVE_BOUNDARY_IDENTITIES: OnceLock<Mutex<BTreeMap<ProcessIdentity, BTreeSet<String>>>> =
    OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static ACTIVE_BOUNDARY_LEADERS: OnceLock<Mutex<BTreeMap<u32, usize>>> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
static SPAWN_REGISTRATIONS: OnceLock<SpawnRegistrationRegistry> = OnceLock::new();
#[cfg(unix)]
static OWNED_CHILD_SPAWN: RwLock<()> = RwLock::new(());
#[cfg(any(target_os = "linux", target_os = "android"))]
static ADOPTED_REAPER_WAITING: AtomicBool = AtomicBool::new(false);
#[cfg(any(target_os = "linux", target_os = "android"))]
static ADOPTED_REAP_COORDINATOR: OnceLock<AdoptedReapCoordinator> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
const PENDING_REAP_BATCH: usize = 64;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
static PORTABLE_SNAPSHOT_CACHE: OnceLock<SnapshotCache> = OnceLock::new();
const BOUNDARY_ENV: &str = "SHDEPS_INTERNAL_PROCESS_BOUNDARIES";

#[cfg(unix)]
pub(crate) const TERMINATE_SIGNAL: i32 = libc::SIGTERM;
#[cfg(not(unix))]
pub(crate) const TERMINATE_SIGNAL: i32 = 15;
#[cfg(unix)]
pub(crate) const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub(crate) const KILL_SIGNAL: i32 = 9;

#[cfg(unix)]
const HANDLED_SIGNALS: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

#[cfg(unix)]
extern "C" fn interrupted(signal: i32) {
    ACTIVE_HANDLERS.fetch_add(1, Ordering::SeqCst);
    record_signal(signal);
    ACTIVE_HANDLERS.fetch_sub(1, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn terminal_exit(signal: i32) {
    let selected = TERMINAL_EXIT_OVERRIDE.load(Ordering::SeqCst);
    let status = if selected > 0 { selected } else { 128 + signal };
    // SAFETY: _exit is async-signal-safe and intentionally skips process
    // teardown after the CLI has finished all owned-resource cleanup.
    unsafe { libc::_exit(status) }
}

fn record_signal(signal: i32) {
    #[cfg(unix)]
    if TERMINAL_HANDOFF_ACTIVE.load(Ordering::SeqCst) {
        terminal_exit(signal);
    }
    if RECEIVED
        .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        #[cfg(unix)]
        if TERMINAL_HANDOFF_ACTIVE.load(Ordering::SeqCst) {
            terminal_exit(signal);
        }
        return;
    }
    #[cfg(unix)]
    if TERMINAL_HANDOFF_ACTIVE.load(Ordering::SeqCst) {
        terminal_exit(signal);
    }
    #[cfg(unix)]
    {
        let writer = LAUNCH_CANCEL_WRITER.load(Ordering::SeqCst);
        if writer >= 0 {
            let byte = [1_u8; 1];
            // SAFETY: write is async-signal-safe, the descriptor is kept open
            // until all installed handlers have returned, and the socket is
            // nonblocking so a duplicate notification cannot stall a handler.
            unsafe {
                libc::write(writer, byte.as_ptr().cast(), byte.len());
            }
        }
    }
}

/// Process-local owner for standalone CLI signal dispositions.
///
/// Embedded library calls remain signal-neutral; only `src/main.rs` installs
/// this guard. The first handled signal wins and is returned as `128+signal`
/// after all owned subprocesses have been stopped and drained.
pub struct Signals {
    #[cfg(unix)]
    previous: Vec<(i32, libc::sigaction)>,
    #[cfg(unix)]
    previous_sigchld: Option<libc::sigaction>,
    #[cfg(unix)]
    launch_cancel_reader: Option<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    launch_cancel_writer: Option<std::os::unix::net::UnixStream>,
    _owner: Option<MutexGuard<'static, ()>>,
    restore: bool,
    active: bool,
}

impl Signals {
    /// Installs process-entry signal ownership and descendant adoption.
    pub fn install() -> std::io::Result<Self> {
        let mut guard = Self::install_with_restore(true)?;
        adopt_descendants()?;
        // The standalone entrypoint transfers these dispositions directly to
        // `exit_process`; it must not restore caller dispositions on Drop.
        guard.restore = false;
        Ok(guard)
    }

    // Serializes ownership and saves every disposition changed by this guard.
    fn install_with_restore(restore: bool) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::fd::AsRawFd as _;

        let owner = SIGNAL_OWNER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        RECEIVED.store(0, Ordering::SeqCst);
        CLEANUP_DIAGNOSTICS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        CLEANUP_FAILED.store(false, Ordering::SeqCst);
        #[cfg(unix)]
        let (launch_cancel_reader, launch_cancel_writer) = {
            let pair = std::os::unix::net::UnixStream::pair()?;
            pair.0.set_nonblocking(true)?;
            pair.1.set_nonblocking(true)?;
            pair
        };
        let mut guard = Self {
            #[cfg(unix)]
            previous: Vec::new(),
            #[cfg(unix)]
            previous_sigchld: None,
            #[cfg(unix)]
            launch_cancel_reader: Some(launch_cancel_reader),
            #[cfg(unix)]
            launch_cancel_writer: Some(launch_cancel_writer),
            _owner: Some(owner),
            restore: true,
            active: true,
        };

        #[cfg(unix)]
        guard.install_supervisor_sigchld()?;

        #[cfg(unix)]
        for signal in HANDLED_SIGNALS {
            // SAFETY: zero initialization is valid for sigaction, the mask is
            // initialized explicitly, and saved actions outlive each call.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = interrupted as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                guard.previous.push((signal, previous));
            }
        }

        #[cfg(unix)]
        {
            LAUNCH_CANCEL_READER.store(
                guard
                    .launch_cancel_reader
                    .as_ref()
                    .expect("launch cancellation reader available")
                    .as_raw_fd(),
                Ordering::SeqCst,
            );
            LAUNCH_CANCEL_WRITER.store(
                guard
                    .launch_cancel_writer
                    .as_ref()
                    .expect("launch cancellation writer available")
                    .as_raw_fd(),
                Ordering::SeqCst,
            );
        }
        SIGNALS_ACTIVE.store(true, Ordering::SeqCst);
        guard.restore = restore;
        Ok(guard)
    }

    #[cfg(unix)]
    fn install_supervisor_sigchld(&mut self) -> std::io::Result<()> {
        // A caller may ignore SIGCHLD or request SA_NOCLDWAIT. Either setting
        // lets the kernel auto-reap children before the supervisor can retain
        // their status. Keep SIGCHLD at its default disposition while Shdeps
        // owns child lifecycles, but remember the caller's exact action for
        // user commands and for embedded-call restoration.
        unsafe {
            let mut default: libc::sigaction = std::mem::zeroed();
            let mut previous: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            if libc::sigemptyset(&mut default.sa_mask) != 0
                || libc::sigaction(libc::SIGCHLD, &default, &mut previous) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            self.previous_sigchld = Some(previous);
            *TARGET_SIGCHLD
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(previous);
        }
        Ok(())
    }

    /// Applies signal precedence to the completed CLI result.
    pub fn finish_result<E>(
        mut self,
        result: std::result::Result<i32, E>,
    ) -> std::result::Result<i32, E> {
        match self.close() {
            Some(_) if CLEANUP_FAILED.load(Ordering::SeqCst) => Ok(1),
            Some(signal) => Ok(128 + signal),
            None => result,
        }
    }

    /// Selects the standalone CLI status and exits without a signal-loss gap.
    pub fn exit_process(self, fallback: i32) -> ! {
        #[cfg(unix)]
        self.exit_process_with(fallback, |_| {});

        #[cfg(not(unix))]
        {
            let code = self
                .finish_result::<std::convert::Infallible>(Ok(fallback))
                .expect("infallible fallback result");
            std::process::exit(code);
        }
    }

    #[cfg(unix)]
    fn exit_process_with(mut self, fallback: i32, mut after_disposition: impl FnMut(i32)) -> ! {
        let status = match self.prepare_terminal_exit(fallback, &mut after_disposition) {
            Ok(status) => status,
            Err(error) => {
                TERMINAL_EXIT_OVERRIDE.store(1, Ordering::SeqCst);
                eprintln!("error: could not finalize signal handling: {error}");
                1
            }
        };
        // SAFETY: all user-visible output is flushed by the process entrypoint
        // before this call. No Rust destructor may reopen the signal window.
        unsafe { libc::_exit(status) }
    }

    #[cfg(unix)]
    fn prepare_terminal_exit(
        &mut self,
        fallback: i32,
        after_disposition: &mut impl FnMut(i32),
    ) -> std::io::Result<i32> {
        let initial_signal = received_signal();
        TERMINAL_EXIT_OVERRIDE.store(
            if CLEANUP_FAILED.load(Ordering::SeqCst) {
                1
            } else {
                initial_signal.map_or(0, |signal| 128 + signal)
            },
            Ordering::SeqCst,
        );
        TERMINAL_HANDOFF_ACTIVE.store(true, Ordering::SeqCst);
        let blocked = handled_signal_set()?;
        // SAFETY: blocked is initialized and pthread_sigmask writes the prior
        // mask into the local storage. This process is about to exit, so the
        // previous mask does not need to be restored.
        unsafe {
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
            if result != 0 {
                return Err(std::io::Error::from_raw_os_error(result));
            }
        }

        for signal in HANDLED_SIGNALS {
            // SAFETY: terminal_exit is async-signal-safe, and the signal is
            // blocked on this thread until all dispositions are installed.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = terminal_exit as *const () as usize;
                if libc::sigemptyset(&mut action.sa_mask) != 0
                    || libc::sigaction(signal, &action, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            after_disposition(signal);
        }
        while ACTIVE_HANDLERS.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }

        LAUNCH_CANCEL_READER.store(-1, Ordering::SeqCst);
        LAUNCH_CANCEL_WRITER.store(-1, Ordering::SeqCst);
        self.launch_cancel_reader.take();
        self.launch_cancel_writer.take();
        let first_signal = match RECEIVED.swap(SIGNAL_CLOSED, Ordering::SeqCst) {
            signal if signal > 0 => Some(signal),
            _ => None,
        };
        SIGNALS_ACTIVE.store(false, Ordering::SeqCst);
        self.previous.clear();
        self.previous_sigchld.take();
        *TARGET_SIGCHLD
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.active = false;

        let cleanup_failed = CLEANUP_FAILED.load(Ordering::SeqCst);
        let status = if cleanup_failed {
            1
        } else if let Some(signal) = first_signal {
            128 + signal
        } else {
            fallback
        };
        // Zero leaves a late signal free to select its own conventional
        // status. A cleanup failure or an already-recorded first signal must
        // retain precedence over every later delivery.
        TERMINAL_EXIT_OVERRIDE.store(
            if cleanup_failed || first_signal.is_some() {
                status
            } else {
                0
            },
            Ordering::SeqCst,
        );
        // SAFETY: blocked is initialized. Any pending or newly delivered
        // handled signal now runs terminal_exit; otherwise the caller invokes
        // _exit immediately with status.
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &blocked, std::ptr::null_mut()) };
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result));
        }
        Ok(status)
    }

    /// Takes subprocess-cleanup failures recorded while this signal owner ran.
    ///
    /// The standalone binary emits these before selecting its final status. A
    /// failed delivery, reap, or bounded teardown changes signal completion to
    /// status 1 rather than falsely acknowledging complete cleanup with
    /// `128+signal`.
    pub fn take_cleanup_diagnostics(&self) -> Vec<String> {
        std::mem::take(
            &mut *CLEANUP_DIAGNOSTICS
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    // Closes the handler race and returns the first signal, when present.
    fn close(&mut self) -> Option<i32> {
        if !self.active {
            return None;
        }
        self.ignore();
        while ACTIVE_HANDLERS.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
        #[cfg(unix)]
        {
            LAUNCH_CANCEL_READER.store(-1, Ordering::SeqCst);
            LAUNCH_CANCEL_WRITER.store(-1, Ordering::SeqCst);
            self.launch_cancel_reader.take();
            self.launch_cancel_writer.take();
        }
        let received = match RECEIVED.swap(SIGNAL_CLOSED, Ordering::SeqCst) {
            signal if signal > 0 => Some(signal),
            _ => None,
        };
        SIGNALS_ACTIVE.store(false, Ordering::SeqCst);
        if self.restore {
            self.restore_previous();
        } else {
            #[cfg(unix)]
            {
                self.previous.clear();
                self.previous_sigchld.take();
                *TARGET_SIGCHLD
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = None;
            }
        }
        self.active = false;
        received
    }

    // Prevents new callbacks while the final latch value is collected.
    fn ignore(&self) {
        #[cfg(unix)]
        for &(signal, _) in &self.previous {
            // SAFETY: SIG_IGN is a valid disposition for every handled signal.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    // Restores saved dispositions when setup cannot establish lifetime ownership.
    fn restore_previous(&mut self) {
        #[cfg(unix)]
        for (signal, action) in self.previous.drain(..) {
            // SAFETY: restore the exact initialized action saved at install.
            unsafe {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        #[cfg(unix)]
        if let Some(action) = self.previous_sigchld.take() {
            // SAFETY: restore the exact action captured before supervisor
            // ownership, after every owned child has been reaped.
            unsafe {
                libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut());
            }
        }
        #[cfg(unix)]
        {
            *TARGET_SIGCHLD
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }
}

#[cfg(unix)]
fn handled_signal_set() -> std::io::Result<libc::sigset_t> {
    // SAFETY: sigemptyset initializes the complete value before sigaddset.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for signal in HANDLED_SIGNALS {
            if libc::sigaddset(&mut set, signal) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(set)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Returns the first signal captured by the process-entry owner.
pub(crate) fn received_signal() -> Option<i32> {
    match RECEIVED.load(Ordering::SeqCst) {
        signal if signal > 0 => Some(signal),
        _ => None,
    }
}

/// Refuses to begin another subprocess after cancellation was requested.
pub(crate) fn check() -> std::io::Result<()> {
    if received_signal().is_some() {
        Err(std::io::Error::other("interrupted by signal"))
    } else {
        Ok(())
    }
}

/// Returns whether this runtime can enforce truthful owned-subprocess
/// cancellation acknowledgement.
///
/// Every Unix implementation can safely signal the retained child group and
/// fail closed when an escaped member cannot be addressed atomically. The
/// capability promises that `128+signal` means cleanup was proven complete;
/// it does not promise that every kernel can successfully clean every
/// deliberately reparented topology. Incomplete cleanup is instead reported as
/// an ordinary failure with diagnostics, never as signal acknowledgement.
pub fn owned_subprocess_cancellation_available() -> bool {
    #[cfg(unix)]
    {
        true
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn exact_descendant_authority_available() -> bool {
    *EXACT_DESCENDANT_AUTHORITY.get_or_init(|| {
        runtime_cancellation_capability_with(
            || {
                stable_pidfd(open_pidfd(std::process::id())?)?.ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "pidfd_open could not retain the current process",
                    )
                })
            },
            |pidfd| {
                signal_pidfd(pidfd, 0)?.then_some(()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "pidfd_send_signal could not address the retained process",
                    )
                })
            },
            probe_pidfd_wait,
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn runtime_cancellation_capability_with<T>(
    open: impl FnOnce() -> std::io::Result<T>,
    signal: impl FnOnce(&T) -> std::io::Result<()>,
    wait: impl FnOnce(&T) -> std::io::Result<()>,
) -> bool {
    let Ok(handle) = open() else {
        return false;
    };
    signal(&handle).and_then(|()| wait(&handle)).is_ok()
}

pub(crate) fn record_cleanup_error(error: &std::io::Error) {
    CLEANUP_FAILED.store(true, Ordering::SeqCst);
    let diagnostic = error.to_string();
    // TEMP-DIAG-131: report every recorded cleanup error. Revert with the
    // rest of the macOS teardown telemetry once macOS is green.
    #[cfg(test)]
    if teardown_diag_enabled() {
        eprintln!(
            "DIAG131 cleanup_error: {diagnostic} test={}",
            diag_child_name()
        );
    }
    let mut diagnostics = CLEANUP_DIAGNOSTICS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

/// TEMP-DIAG-131: teardown failure/hang telemetry for the macOS signal
/// tests. Env-gated, stderr-unbuffered so killed subprocesses
/// still leave their last heartbeat. Revert once macOS is green.
#[cfg(unix)]
pub(crate) fn teardown_diag_enabled() -> bool {
    std::env::var_os("SHDEPS_TEST_TEARDOWN_DIAG").is_some()
}

/// TEMP-DIAG-131: attribute every marker with the owning signal-boundary
/// child. Each child runs one `--exact TEST` behind `--test-threads=1`,
/// so the argv filter names the test that emitted the marker; `?` marks
/// emissions from a process without that filter. Cached: argv never
/// changes after process start. Revert with the macOS teardown telemetry
/// once macOS is green.
#[cfg(any(test, unix))]
pub(crate) fn diag_child_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let mut args = std::env::args();
        let mut name = String::from("?");
        while let Some(arg) = args.next() {
            if arg == "--exact" {
                if let Some(exact) = args.next() {
                    name = exact.rsplit("::").next().unwrap_or("?").to_owned();
                }
                break;
            }
        }
        name
    })
    .clone()
}

/// TEMP-DIAG-131: prints scope entry/exit (see above).
#[cfg(test)]
struct DiagScope {
    name: &'static str,
    started: Instant,
}

#[cfg(test)]
impl DiagScope {
    fn enter(name: &'static str) -> Self {
        if teardown_diag_enabled() {
            eprintln!("DIAG131 enter {name} test={}", diag_child_name());
        }
        Self {
            name,
            started: Instant::now(),
        }
    }
}

#[cfg(test)]
impl Drop for DiagScope {
    fn drop(&mut self) {
        if teardown_diag_enabled() {
            eprintln!(
                "DIAG131 exit {} elapsed_ms={} test={}",
                self.name,
                self.started.elapsed().as_millis(),
                diag_child_name()
            );
        }
    }
}

/// TEMP-DIAG-131: child-side liveness watchdog for the macOS timeout
/// victims. A detached thread prints an attributed heartbeat proving the
/// child is alive and whether its signal latch fired; killed children
/// leave their last tick behind. Revert with the macOS teardown
/// telemetry once macOS is green.
#[cfg(all(test, unix))]
pub(crate) fn spawn_teardown_watchdog(test_name: &'static str) -> impl Drop {
    struct Watchdog {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for Watchdog {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_thread = std::sync::Arc::clone(&stop);
    let started = Instant::now();
    std::thread::spawn(move || {
        let mut tick = 0_u32;
        while !stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            tick += 1;
            if teardown_diag_enabled() {
                eprintln!(
                    "DIAG131 watchdog {test_name} tick={tick} elapsed_ms={} received={:?} active_handlers={}",
                    started.elapsed().as_millis(),
                    received_signal(),
                    ACTIVE_HANDLERS.load(std::sync::atomic::Ordering::SeqCst),
                );
            }
        }
    });
    Watchdog { stop }
}

/// TEMP-DIAG-131: attributed phase marker for the macOS timeout
/// victims. Revert with the macOS teardown telemetry once macOS is green.
#[cfg(unix)]
pub(crate) fn teardown_phase(test_name: &str, phase: &str) {
    if teardown_diag_enabled() {
        eprintln!(
            "DIAG131 phase {test_name} {phase} test={}",
            diag_child_name()
        );
    }
}

/// Runs one foreground-compatible child in an owned process group.
///
/// Commands such as interactive sudo need the caller's controlling terminal,
/// while cancellation must never signal the caller or the user's shell. The child
/// therefore gets a dedicated group in the same session and temporarily owns
/// the terminal foreground while it runs. Both output pipes are drained
/// concurrently before return.
pub(crate) fn output(
    command: Command,
    stdin_bytes: Option<&[u8]>,
) -> std::io::Result<std::process::Output> {
    output_with_attribution(command, stdin_bytes, true)
}

pub(crate) fn output_without_attribution(
    command: Command,
    stdin_bytes: Option<&[u8]>,
) -> std::io::Result<std::process::Output> {
    output_with_attribution(command, stdin_bytes, false)
}

pub(crate) fn spawn_owned(
    command: &mut Command,
    isolation: Isolation,
    attribute_descendants: bool,
) -> std::io::Result<OwnedChild> {
    spawn_owned_with_foreground(command, isolation, attribute_descendants, true)
}

fn spawn_owned_with_foreground(
    command: &mut Command,
    isolation: Isolation,
    attribute_descendants: bool,
    acquire_foreground: bool,
) -> std::io::Result<OwnedChild> {
    check()?;
    let marker = isolate_with_attribution(command, isolation, attribute_descendants);
    #[cfg(unix)]
    let _spawn = spawn_registration_guard()?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _registration = SpawnRegistrationWindow::begin();
    let child = command.spawn()?;
    let mut child = OwnedChild::new_with_deferred_foreground(child, isolation, marker);
    if acquire_foreground {
        child.acquire_foreground();
    }
    Ok(child)
}

enum PendingStdin {
    #[cfg(unix)]
    Nonblocking {
        stdin: std::process::ChildStdin,
        bytes: Vec<u8>,
        written: usize,
    },
    #[cfg(not(unix))]
    Thread(Option<std::thread::JoinHandle<std::io::Result<()>>>),
}

impl PendingStdin {
    fn new(
        stdin: std::process::ChildStdin,
        bytes: Vec<u8>,
        activity: std::sync::mpsc::Sender<()>,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;

            let _ = activity;
            // SAFETY: fcntl only reads or updates the flags on this live pipe
            // descriptor. Keeping the original flags preserves any platform
            // behavior other than adding nonblocking writes.
            unsafe {
                let flags = libc::fcntl(stdin.as_raw_fd(), libc::F_GETFL);
                if flags == -1
                    || libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(Self::Nonblocking {
                stdin,
                bytes,
                written: 0,
            })
        }
        #[cfg(not(unix))]
        {
            use std::io::Write as _;

            let mut stdin = stdin;
            Ok(Self::Thread(Some(std::thread::spawn(move || {
                let result = stdin.write_all(&bytes);
                let _ = activity.send(());
                result
            }))))
        }
    }

    // Advances input without ever blocking the supervision thread. Returning
    // true transfers no resources: the caller then drops this value, closing
    // stdin and delivering EOF to the child.
    fn poll(&mut self) -> std::io::Result<bool> {
        match self {
            #[cfg(unix)]
            Self::Nonblocking {
                stdin,
                bytes,
                written,
            } => write_available(stdin, bytes, written),
            #[cfg(not(unix))]
            Self::Thread(handle) => {
                if !handle
                    .as_ref()
                    .is_some_and(std::thread::JoinHandle::is_finished)
                {
                    return Ok(false);
                }
                join_writer(handle.take().expect("finished writer available"))?;
                Ok(true)
            }
        }
    }
}

#[cfg(unix)]
fn write_available(
    writer: &mut impl std::io::Write,
    bytes: &[u8],
    written: &mut usize,
) -> std::io::Result<bool> {
    loop {
        if *written == bytes.len() {
            return Ok(true);
        }
        match writer.write(&bytes[*written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write subprocess stdin",
                ));
            }
            Ok(count) => *written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

fn output_with_attribution(
    mut command: Command,
    stdin_bytes: Option<&[u8]>,
    attribute_descendants: bool,
) -> std::io::Result<std::process::Output> {
    // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
    #[cfg(test)]
    let _diag = DiagScope::enter("output");
    use std::io::Read as _;

    command
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_owned_with_foreground(
        &mut command,
        Isolation::ExactChild,
        attribute_descendants,
        false,
    )?;
    let stdin = child.take_stdin();
    let stdout = child.take_stdout().expect("stdout configured as piped");
    let stderr = child.take_stderr().expect("stderr configured as piped");
    let (activity_sender, activity) = std::sync::mpsc::channel();
    let stdout_activity = activity_sender.clone();
    let stdout_task: OutputReadTask = Box::new(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_activity.send(());
        result
    });
    let stderr_activity = activity_sender.clone();
    let stderr_task: OutputReadTask = Box::new(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stderr_activity.send(());
        result
    });
    let (stdout_reader, stderr_reader) =
        spawn_output_readers(&mut child, stdout_task, stderr_task)?;
    // Keep Unix input nonblocking on the supervision thread. Cancellation can
    // then close the only writer immediately, with no detached thread or file
    // descriptor left behind. Platforms without Unix fd flags retain the
    // background writer used by their signal-neutral process runner.
    let mut stdin_writer = match (stdin, stdin_bytes) {
        (Some(stdin), Some(bytes)) => Some(PendingStdin::new(
            stdin,
            bytes.to_vec(),
            activity_sender.clone(),
        )?),
        _ => None,
    };
    // Keep one sender alive until supervision ends. Once both capture readers
    // close, a disconnected receiver returns immediately and would otherwise
    // turn the fallback wait into a busy loop while the child remains alive.
    let _activity_lifetime = activity_sender;

    if let Some(writer) = &mut stdin_writer {
        if writer.poll()? {
            stdin_writer.take();
        }
    }
    child.acquire_foreground();

    let status = loop {
        if received_signal().is_some() {
            // On Unix this synchronously closes the nonblocking pipe. No
            // writer thread or descriptor can outlive this function.
            #[cfg(unix)]
            drop(stdin_writer.take());
            let stopped = child.stop(TERMINATE_SIGNAL);
            let drain_deadline = Instant::now() + GRACE;
            while !(stdout_reader.is_finished() && stderr_reader.is_finished())
                && Instant::now() < drain_deadline
            {
                let _ = activity.recv_timeout(OUTPUT_POLL);
            }
            // Never let an inherited descriptor held by an unobservable or
            // uninterruptible descendant turn cancellation into an unbounded
            // reader join. Finished readers are still joined so their stacks
            // and I/O errors are accounted for; an unfinished handle detaches
            // but records incomplete cleanup, so signal acknowledgement fails
            // closed instead of substituting empty output.
            if stdout_reader.is_finished() {
                let _ = join_output_reader(stdout_reader, "stdout");
            } else {
                let _ = unfinished_output_reader("stdout");
            }
            if stderr_reader.is_finished() {
                let _ = join_output_reader(stderr_reader, "stderr");
            } else {
                let _ = unfinished_output_reader("stderr");
            }
            #[cfg(not(unix))]
            if let Some(writer) = stdin_writer.take() {
                let _ = match writer {
                    PendingStdin::Thread(Some(writer)) if writer.is_finished() => {
                        join_writer(writer)
                    }
                    PendingStdin::Thread(_) => unfinished_stdin_writer(),
                };
            }
            stopped?;
            return Err(std::io::Error::other("interrupted by signal"));
        }

        if let Some(writer) = &mut stdin_writer {
            match writer.poll() {
                Ok(true) => {
                    stdin_writer.take();
                }
                Ok(false) => {}
                Err(error) => {
                    let _ = child.stop(KILL_SIGNAL);
                    let drain_deadline = Instant::now() + GRACE;
                    while !(stdout_reader.is_finished() && stderr_reader.is_finished())
                        && Instant::now() < drain_deadline
                    {
                        let _ = activity.recv_timeout(OUTPUT_POLL);
                    }
                    if stdout_reader.is_finished() {
                        let _ = join_output_reader(stdout_reader, "stdout");
                    } else {
                        let _ = unfinished_output_reader("stdout");
                    }
                    if stderr_reader.is_finished() {
                        let _ = join_output_reader(stderr_reader, "stderr");
                    } else {
                        let _ = unfinished_output_reader("stderr");
                    }
                    return Err(error);
                }
            }
        }

        let output_drained =
            stdin_writer.is_none() && stdout_reader.is_finished() && stderr_reader.is_finished();
        if output_drained {
            if let Some(status) = child.wait_if_exited_and_output_drained()? {
                break status;
            }
        } else {
            let _ = child.exited()?;
        }
        let _ = activity.recv_timeout(OUTPUT_POLL);
    };
    let stdout = join_output_reader(stdout_reader, "stdout");
    let stderr = join_output_reader(stderr_reader, "stderr");
    let stdout = stdout?;
    let stderr = stderr?;
    check()?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

// Joins one capture reader without letting a thread panic escape as success.
pub(crate) fn join_output_reader(
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> std::io::Result<Vec<u8>> {
    let result = reader
        .join()
        .map_err(|_| std::io::Error::other(format!("{stream} output reader panicked")))
        .and_then(|result| result);
    if let Err(error) = &result {
        record_cleanup_error(error);
    }
    result
}

pub(crate) fn unfinished_output_reader(stream: &str) -> std::io::Result<Vec<u8>> {
    let error = std::io::Error::other(format!(
        "{stream} output reader did not finish within the cleanup deadline"
    ));
    record_cleanup_error(&error);
    Err(error)
}

pub(crate) type OutputReader = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;
pub(crate) type OutputReadTask = Box<dyn FnOnce() -> std::io::Result<Vec<u8>> + Send + 'static>;

fn spawn_output_reader(name: &str, read: OutputReadTask) -> std::io::Result<OutputReader> {
    std::thread::Builder::new()
        .name(format!("shdeps-{name}-reader"))
        .spawn(read)
}

pub(crate) fn spawn_output_readers(
    child: &mut OwnedChild,
    stdout: OutputReadTask,
    stderr: OutputReadTask,
) -> std::io::Result<(OutputReader, OutputReader)> {
    spawn_output_readers_with(child, stdout, stderr, spawn_output_reader)
}

fn spawn_output_readers_with(
    child: &mut OwnedChild,
    stdout: OutputReadTask,
    stderr: OutputReadTask,
    mut spawn: impl FnMut(&str, OutputReadTask) -> std::io::Result<OutputReader>,
) -> std::io::Result<(OutputReader, OutputReader)> {
    let stdout = match spawn("stdout", stdout) {
        Ok(reader) => reader,
        Err(error) => return Err(reader_setup_failed(child, None, error)),
    };
    let stderr = match spawn("stderr", stderr) {
        Ok(reader) => reader,
        Err(error) => {
            return Err(reader_setup_failed(child, Some((stdout, "stdout")), error));
        }
    };
    Ok((stdout, stderr))
}

pub(crate) fn reader_setup_failed(
    child: &mut OwnedChild,
    started: Option<(OutputReader, &'static str)>,
    error: std::io::Error,
) -> std::io::Error {
    let kind = error.kind();
    let mut details = error.to_string();
    if let Err(cleanup) = child.stop(KILL_SIGNAL) {
        details.push_str(&format!("; subprocess cleanup failed: {cleanup}"));
    }
    if let Some((reader, stream)) = started {
        let deadline = std::time::Instant::now() + GRACE;
        while !reader.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(OUTPUT_POLL);
        }
        let drained = if reader.is_finished() {
            join_output_reader(reader, stream).map(|_| ())
        } else {
            unfinished_output_reader(stream).map(|_| ())
        };
        if let Err(drain) = drained {
            details.push_str(&format!("; subprocess output cleanup failed: {drain}"));
        }
    }
    std::io::Error::new(kind, details)
}

#[cfg(not(unix))]
fn join_writer(writer: std::thread::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    writer
        .join()
        .map_err(|_| std::io::Error::other("subprocess stdin writer panicked"))
        .and_then(|result| result)
}

#[cfg(not(unix))]
fn unfinished_stdin_writer() -> std::io::Result<()> {
    let error =
        std::io::Error::other("subprocess stdin writer did not finish within the cleanup deadline");
    record_cleanup_error(&error);
    Err(error)
}

/// Isolation topology owned by one subprocess invocation.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Isolation {
    /// The child retains the caller's session in an owned foreground group.
    ExactChild,
    /// The child leads a new session whose members can be discovered by SID.
    DetachedSession,
    /// The child remains in the caller's session but leads a new process group.
    ParentSession,
}

/// Opaque inherited attribution carried by one owned subprocess tree.
pub(crate) struct BoundaryMarker {
    token: Option<String>,
    #[cfg(unix)]
    lifetime_reader: Option<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    lifetime_writer: Option<std::os::unix::net::UnixStream>,
}

impl BoundaryMarker {
    fn install(command: &mut Command, enabled: bool) -> Self {
        if !enabled {
            return Self::without_lifetime(None);
        }
        let Some(process_nonce) = boundary_process_nonce() else {
            // Without kernel randomness, topology remains usable but an
            // orphaned process must not be attributed with a predictable token
            // that could collide after PID reuse.
            return Self::without_lifetime(None);
        };
        let token = boundary_token(
            process_nonce,
            BOUNDARY_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        // Preserve ancestor markers across recursive Shdeps invocations so an
        // outer supervisor can still identify descendants of the nested one.
        // Ignore malformed or unexpectedly large inherited values: this is an
        // attribution aid, never an authority supplied by the caller.
        let inherited = std::env::var(BOUNDARY_ENV).ok().filter(|value| {
            value.len() <= 4096
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
        });
        let chain =
            inherited.map_or_else(|| token.clone(), |inherited| format!("{inherited}:{token}"));
        command.env(BOUNDARY_ENV, chain);
        Self::without_lifetime(Some(token))
    }

    fn without_lifetime(token: Option<String>) -> Self {
        Self {
            token,
            #[cfg(unix)]
            lifetime_reader: None,
            #[cfg(unix)]
            lifetime_writer: None,
        }
    }
}

fn boundary_process_nonce() -> Option<&'static str> {
    BOUNDARY_PROCESS_NONCE
        .get_or_init(|| {
            use std::io::Read as _;

            let mut bytes = [0_u8; 16];
            let mut random = std::fs::File::open("/dev/urandom").ok()?;
            random.read_exact(&mut bytes).ok()?;
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut encoded = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
            Some(encoded)
        })
        .as_deref()
}

fn boundary_token(process_nonce: &str, counter: usize) -> String {
    format!("{process_nonce}-{counter}")
}

/// Configures the child-side process boundary before `exec`.
#[cfg(test)]
pub(crate) fn isolate(command: &mut Command, isolation: Isolation) -> BoundaryMarker {
    isolate_with_attribution(command, isolation, true)
}

fn isolate_with_attribution(
    command: &mut Command,
    isolation: Isolation,
    attribute_descendants: bool,
) -> BoundaryMarker {
    let mut marker = BoundaryMarker::install(command, attribute_descendants);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;
        let lifetime = std::os::unix::net::UnixStream::pair()
            .and_then(|(reader, writer)| {
                reader.set_nonblocking(true)?;
                Ok((reader, writer))
            })
            .ok();
        let lifetime_writer_fd = lifetime.as_ref().map(|(_, writer)| writer.as_raw_fd());
        let launch_cancel_reader = LAUNCH_CANCEL_READER.load(Ordering::SeqCst);
        let target_sigchld = target_sigchld_disposition();
        // SAFETY: setsid and setpgid are async-signal-safe and this callback
        // executes after fork and before exec. Clearing CLOEXEC only in the
        // child creates an inherited lifetime lease without leaking the
        // parent's writer after spawn.
        unsafe {
            command.pre_exec(move || {
                let result = match isolation {
                    Isolation::ExactChild | Isolation::ParentSession => libc::setpgid(0, 0),
                    Isolation::DetachedSession => libc::setsid(),
                };
                if result == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(fd) = lifetime_writer_fd {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags == -1
                        || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // A signal delivered after the final cancellation check must
                // not run the parent's copied handler and disappear when exec
                // resets that handler. Reset every handled disposition first:
                // earlier deliveries publish to the inherited self-pipe and
                // later deliveries terminate the child before arbitrary code.
                reset_child_signal_dispositions()?;
                if launch_cancel_reader >= 0 && launch_cancellation_pending(launch_cancel_reader)? {
                    return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
                }
                if let Some(action) = &target_sigchld {
                    if libc::sigaction(libc::SIGCHLD, action, std::ptr::null_mut()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        if let Some((reader, writer)) = lifetime {
            marker.lifetime_reader = Some(reader);
            marker.lifetime_writer = Some(writer);
        }
    }
    #[cfg(not(unix))]
    let _ = (command, isolation);
    marker
}

#[cfg(unix)]
fn target_sigchld_disposition() -> Option<libc::sigaction> {
    TARGET_SIGCHLD
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|action| unsafe {
            // sigaction is a plain C value copied while the process-wide
            // signal owner prevents concurrent replacement.
            std::ptr::read(action)
        })
}

#[cfg(unix)]
fn reset_child_signal_dispositions() -> std::io::Result<()> {
    for signal in HANDLED_SIGNALS {
        // SAFETY: the child is single-threaded between fork and exec. SIG_DFL
        // and an empty handler mask are valid for every handled signal.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            if libc::sigemptyset(&mut action.sa_mask) != 0
                || libc::sigaction(signal, &action, std::ptr::null_mut()) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn launch_cancellation_pending(reader: i32) -> std::io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: reader,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: pollfd points to one initialized entry and timeout zero makes
        // this an immediate child-side authorization check before exec.
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result > 0 {
            return Ok(pollfd.revents
                & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// RAII owner for a child process boundary.
pub(crate) struct OwnedChild {
    child: Option<Child>,
    isolation: Isolation,
    #[cfg(unix)]
    foreground: Option<TerminalForeground>,
    #[cfg(unix)]
    boundary: Boundary,
    #[cfg(unix)]
    next_observation: Instant,
    leader_exited: bool,
    cleanup_complete: bool,
}

impl OwnedChild {
    /// Takes ownership after the caller has removed any piped descriptors.
    #[cfg(test)]
    pub(crate) fn new(child: Child, isolation: Isolation, marker: BoundaryMarker) -> Self {
        let mut child = Self::new_with_deferred_foreground(child, isolation, marker);
        child.acquire_foreground();
        child
    }

    fn new_with_deferred_foreground(
        child: Child,
        isolation: Isolation,
        marker: BoundaryMarker,
    ) -> Self {
        #[cfg(unix)]
        let boundary = Boundary::new(child.id(), isolation, marker);
        #[cfg(not(unix))]
        let _ = marker;
        Self {
            child: Some(child),
            isolation,
            #[cfg(unix)]
            foreground: None,
            #[cfg(unix)]
            boundary,
            #[cfg(unix)]
            next_observation: Instant::now()
                + if cfg!(any(target_os = "linux", target_os = "android")) {
                    LINUX_SNAPSHOT_TTL
                } else {
                    TRACK_POLL
                },
            leader_exited: false,
            cleanup_complete: false,
        }
    }

    fn acquire_foreground(&mut self) {
        #[cfg(unix)]
        if self.foreground.is_none() && matches!(self.isolation, Isolation::ExactChild) {
            self.foreground = self
                .child
                .as_ref()
                .map(Child::id)
                .and_then(TerminalForeground::give_to);
        }
    }

    pub(crate) fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }

    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }

    pub(crate) fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    /// Observes leader exit without releasing its PID/session identity.
    pub(crate) fn exited(&mut self) -> std::io::Result<bool> {
        if self.leader_exited {
            #[cfg(unix)]
            self.observe_boundary();
            return Ok(true);
        }
        #[cfg(unix)]
        self.observe_boundary();
        let child_id = self.child.as_ref().expect("owned child available").id();
        let termination = observe_exit(self.child.as_mut().expect("owned child available"))?;
        #[cfg(unix)]
        if let Some(ObservedTermination::Stopped(signal)) = termination {
            consume_stopped(child_id)?;
            if matches!(self.isolation, Isolation::ExactChild) {
                let boundary = &self.boundary;
                if let Some(foreground) = &mut self.foreground {
                    foreground.suspend_and_resume(signal, boundary)?;
                } else if matches!(signal, libc::SIGTTIN | libc::SIGTTOU) {
                    suspend_background_job(signal)?;
                    if received_signal().is_none() {
                        self.acquire_foreground();
                        if self.foreground.is_none() {
                            return Err(std::io::Error::other(
                                "resumed background job could not acquire the terminal foreground",
                            ));
                        }
                        signal_group(child_id, libc::SIGCONT)?;
                    }
                }
            }
            return Ok(false);
        }
        let leader_exited = termination.is_some();
        if matches!(self.isolation, Isolation::ExactChild) {
            let foreground = self.foreground_signal_active(termination)?;
            record_observed_child_signal(termination, foreground);
        }
        #[cfg(unix)]
        if leader_exited {
            // A leader can fork and exit between periodic observations. Force
            // a fresh snapshot at that boundary so escaped descendants retain
            // attribution even though portable hot-path scans are shared. Do
            // not reap and lose the leader identity if attribution timed out.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let observed = self
                .boundary
                .observe_leader_exit(Instant::now() + LEADER_EXIT_SNAPSHOT_BUDGET);
            // Portable snapshots spawn `ps` plus per-PID identity probes, so
            // they need the same leader-exit budget as the procfs path; the
            // tighter track budget expires under CI load and fails the
            // completion proof for a leader that already exited cleanly.
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
            let observed = self
                .boundary
                .track(Instant::now() + LEADER_EXIT_SNAPSHOT_BUDGET)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "portable process snapshot deadline expired",
                    )
                });
            // A zombie leader still reserves its PID and process group for
            // safe cleanup, but it can no longer use the terminal. A retained
            // descendant may have taken the foreground in the meantime; only
            // reclaim that group after the fresh boundary observation proves
            // it still belongs to this lease.
            if let Some(foreground) = &self.foreground {
                foreground.restore_if_owned(&self.boundary)?;
            }
            observed.map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not snapshot owned subprocesses after leader exit: {error}"),
                )
            })?;
            self.leader_exited = true;
        }
        Ok(leader_exited)
    }

    /// Reaps a completed non-terminal leader once all of its output has
    /// drained, then proves whether the subreaper has any remaining children.
    ///
    /// Keeping the leader as a zombie is necessary while output remains open:
    /// its PID still anchors process-group/session ownership. Once every pipe
    /// is closed, Linux can reap that leader and use `waitid(P_ALL)` to prove
    /// the common no-descendant case without a whole-procfs scan. Any other
    /// child (including a live, marker-cleared adoptee) forces the strict fresh
    /// attribution path. A job with a terminal lease retains the pre-reap path
    /// because the leader identity also anchors terminal restoration.
    pub(crate) fn wait_if_exited_and_output_drained(
        &mut self,
    ) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self
            .foreground
            .as_ref()
            .is_none_or(TerminalForeground::is_active)
            && !self.leader_exited
        {
            let termination = observe_exit(self.child.as_mut().expect("owned child available"))?;
            if termination.is_none() {
                return Ok(None);
            }
            if matches!(termination, Some(ObservedTermination::Stopped(_))) {
                let _ = self.exited()?;
                return Ok(None);
            }

            if matches!(self.isolation, Isolation::ExactChild) {
                let foreground = self.foreground_signal_active(termination)?;
                record_observed_child_signal(termination, foreground);
                if received_signal().is_some() {
                    return Ok(None);
                }
                if let Some(foreground) = &self.foreground {
                    foreground.restore()?;
                }
            }

            let status = wait_leader(
                self.child.as_mut().expect("owned child available"),
                &mut self.boundary,
            )?;
            self.leader_exited = true;
            let empty = match self
                .boundary
                .observe_after_leader_reap(Instant::now() + LEADER_EXIT_SNAPSHOT_BUDGET)
            {
                Ok(empty) => empty,
                Err(error) => {
                    // The leader has already been reaped, so prevent Drop from
                    // treating its numeric PID as retained authority. Known
                    // descendants remain available to the reaped-boundary path.
                    self.child.take();
                    record_cleanup_error(&error);
                    return Err(error);
                }
            };
            let status = self.finish_wait(Ok(status))?;
            if empty {
                if let Some(foreground) = self.foreground.take() {
                    foreground.restore()?;
                }
            }
            return Ok(Some(status));
        }

        if self.exited()? {
            // `exited` can infer an interactive signal. Do not release the
            // retained leader until the next loop iteration has entered the
            // cancellation path.
            if received_signal().is_some() {
                return Ok(None);
            }
            self.wait().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Reaps a normally completed leader and releases ownership.
    pub(crate) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        #[cfg(unix)]
        let result = wait_leader(
            self.child.as_mut().expect("owned child available"),
            &mut self.boundary,
        );
        #[cfg(not(unix))]
        let result = self.child.as_mut().expect("owned child available").wait();
        self.finish_wait(result)
    }

    fn finish_wait(&mut self, result: std::io::Result<ExitStatus>) -> std::io::Result<ExitStatus> {
        if let Ok(status) = &result {
            if matches!(self.isolation, Isolation::ExactChild) {
                record_child_signal(status, self.foreground_active());
            }
            #[cfg(unix)]
            {
                if self.boundary.leader_retained {
                    self.boundary.release_leader();
                }
                remember_members_for_reap(&self.boundary);
            }
            self.child.take();
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                reap_pending_members().map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!("reaping retained subprocess descendants failed: {error}"),
                    )
                })?;
                reap_unobserved_adopted_zombies(Instant::now() + GRACE).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!("reaping adopted subprocess descendants failed: {error}"),
                    )
                })?;
            }
            if received_signal().is_some() {
                #[cfg(unix)]
                {
                    let cleanup = stop_reaped_boundary(
                        &mut self.boundary,
                        TERMINATE_SIGNAL,
                        self.foreground.as_ref(),
                    );
                    let foreground_error = self.release_foreground().err();
                    if let Err(error) = &cleanup {
                        record_cleanup_error(error);
                    }
                    cleanup?;
                    self.cleanup_complete = true;
                    if let Some(error) = foreground_error {
                        record_cleanup_error(&error);
                        return Err(error);
                    }
                }
                return Err(std::io::Error::other("interrupted by signal"));
            }
        }
        result
    }

    /// Stops the complete owned boundary, escalates when needed, and reaps it.
    pub(crate) fn stop(&mut self, first_signal: i32) -> std::io::Result<ExitStatus> {
        let child = self.child.as_mut().expect("owned child available");
        let mut leader_reaped = false;
        #[cfg(unix)]
        let cleanup = stop_boundary(
            child,
            &mut self.boundary,
            first_signal,
            &mut leader_reaped,
            self.foreground.as_ref(),
        );
        #[cfg(not(unix))]
        let cleanup = stop_boundary(child, self.isolation, first_signal, &mut leader_reaped);
        #[cfg(unix)]
        let foreground_error = self.release_foreground().err();
        #[cfg(unix)]
        let result = match (cleanup, foreground_error) {
            (Err(error), _) => Err(error),
            (Ok(_), Some(error)) => Err(error),
            (Ok(status), None) => Ok(status),
        };
        #[cfg(not(unix))]
        let result = cleanup;
        if let Err(error) = &result {
            record_cleanup_error(error);
        }
        if result.is_ok() || leader_reaped {
            self.child.take();
        }
        if result.is_ok() {
            self.cleanup_complete = true;
        }
        result
    }

    #[cfg(unix)]
    fn observe_boundary(&mut self) {
        let now = Instant::now();
        if now < self.next_observation {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Err(error) = reap_pending_members() {
            record_cleanup_error(&error);
        }
        if self.boundary.track(now + TRACK_SNAPSHOT_BUDGET).is_some() && self.leader_exited {
            if let Some(foreground) = &self.foreground {
                if let Err(error) = foreground.restore_if_owned(&self.boundary) {
                    record_cleanup_error(&error);
                }
            }
        }
        self.next_observation = Instant::now() + TRACK_POLL;
    }

    #[cfg(unix)]
    fn release_foreground(&mut self) -> std::io::Result<()> {
        let Some(foreground) = self.foreground.take() else {
            return Ok(());
        };
        if foreground.is_active() {
            return foreground.restore();
        }
        // A retained descendant may have changed process group and taken the
        // terminal since the last hot-path observation. Refresh attribution
        // before deciding whether that foreground group still belongs to this
        // boundary; stale ownership must neither strand the terminal nor
        // reclaim it from an unrelated process.
        self.boundary
            .observe(Instant::now() + TRACK_SNAPSHOT_BUDGET)
            .ok_or_else(|| {
                std::io::Error::other(
                    "could not snapshot owned subprocesses before terminal restoration",
                )
            })?;
        foreground.restore_if_owned(&self.boundary)
    }

    #[cfg(unix)]
    fn foreground_active(&self) -> bool {
        self.foreground
            .as_ref()
            .is_some_and(TerminalForeground::is_active)
    }

    #[cfg(unix)]
    fn foreground_signal_active(
        &mut self,
        termination: Option<ObservedTermination>,
    ) -> std::io::Result<bool> {
        if self.foreground_active() {
            return Ok(true);
        }
        if observed_handled_signal(termination).is_none() {
            return Ok(false);
        }
        let Some(foreground) = self.foreground.as_ref() else {
            return Ok(false);
        };

        // A child can delegate the controlling terminal to a descendant in a
        // different process group. Conventional 128+signal status is a
        // cancellation acknowledgement only when a fresh boundary snapshot
        // proves that the current foreground group still belongs to this
        // exact invocation.
        self.boundary
            .observe(Instant::now() + TRACK_SNAPSHOT_BUDGET)
            .ok_or_else(|| {
                std::io::Error::other(
                    "could not snapshot owned subprocesses before classifying terminal exit",
                )
            })?;
        foreground.owns_current_group(&self.boundary)
    }

    #[cfg(not(unix))]
    fn foreground_active(&self) -> bool {
        false
    }
}

#[cfg(unix)]
struct TerminalForeground {
    tty: std::fs::File,
    previous_group: i32,
    child_group: i32,
    _owner: MutexGuard<'static, ()>,
}

#[cfg(unix)]
fn fresh_process_owns_foreground_group(
    cached: &ProcessInfo,
    fresh: Option<&ProcessInfo>,
    foreground: u32,
) -> bool {
    cached.live
        && cached.pgid == foreground
        && fresh.is_some_and(|fresh| {
            fresh.identity == cached.identity && fresh.live && fresh.pgid == foreground
        })
}

#[cfg(unix)]
impl TerminalForeground {
    // Give an owned foreground-compatible process group the controlling TTY
    // only when Shdeps currently owns the foreground. Commands without a TTY
    // need no handoff; callers that launched Shdeps in the background retain
    // their existing job-control policy.
    fn give_to(child: u32) -> Option<Self> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let child_group = i32::try_from(child).ok()?;
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY)
            .open("/dev/tty")
            .ok()?;
        let owner = TERMINAL_OWNER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if received_signal().is_some() {
            return None;
        }
        // SAFETY: tcgetpgrp only inspects this valid terminal descriptor.
        let previous_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        // SAFETY: getpgrp has no arguments or pointer preconditions.
        if previous_group <= 0 || previous_group != unsafe { libc::getpgrp() } {
            return None;
        }
        if set_terminal_group(tty.as_raw_fd(), child_group).is_err() {
            return None;
        }
        // A child can reach `/dev/tty` after setpgid but before this handoff.
        // Resume only an observed terminal-I/O stop; an unconditional SIGCONT
        // would erase a deliberate Ctrl-Z that raced startup.
        if observe_stopped(child)
            .ok()
            .flatten()
            .is_some_and(|signal| matches!(signal, libc::SIGTTIN | libc::SIGTTOU))
        {
            let _ = consume_stopped(child);
            let _ = signal_group(child, libc::SIGCONT);
        }
        Some(Self {
            tty,
            previous_group,
            child_group,
            _owner: owner,
        })
    }

    fn is_active(&self) -> bool {
        use std::os::fd::AsRawFd as _;
        // SAFETY: tcgetpgrp only inspects this live terminal descriptor.
        unsafe { libc::tcgetpgrp(self.tty.as_raw_fd()) == self.child_group }
    }

    fn restore(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // Restore only the foreground state this lease still owns. A caller
        // that deliberately transferred the terminal elsewhere wins.
        if self.is_active() {
            set_terminal_group(self.tty.as_raw_fd(), self.previous_group)?;
        }
        Ok(())
    }

    fn restore_if_owned(&self, boundary: &Boundary) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;

        if self.owns_current_group(boundary)? {
            set_terminal_group(self.tty.as_raw_fd(), self.previous_group)?;
        }
        Ok(())
    }

    fn owns_current_group(&self, boundary: &Boundary) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd as _;

        // A child can hand the terminal to another group in its retained
        // boundary. Reclaim that group when a fresh observation and immediate
        // identity check still attribute at least one live member to it. If an
        // unrelated caller changed the foreground, leave that newer lease
        // untouched.
        let foreground = unsafe { libc::tcgetpgrp(self.tty.as_raw_fd()) };
        let mut owned = foreground == self.child_group;
        if !owned && foreground > 0 {
            for process in boundary
                .current
                .values()
                .filter(|process| process.live && process.pgid == foreground as u32)
            {
                let fresh = boundary.revalidated_process(process.pid)?;
                if fresh_process_owns_foreground_group(process, fresh.as_ref(), foreground as u32) {
                    owned = true;
                    break;
                }
            }
        }
        Ok(owned)
    }

    fn suspend_and_resume(
        &mut self,
        child_signal: i32,
        boundary: &Boundary,
    ) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;

        self.restore_if_owned(boundary)?;
        // Preserve the terminal stop reason under a real job-control shell.
        // A session leader has no parent shell in its session, so job-control
        // stop signals would be discarded for its orphaned group; use SIGSTOP
        // for that private-session shape (including PTY harnesses).
        let own_pid = std::process::id() as i32;
        // SAFETY: getsid only inspects this process. `previous_group` was
        // captured as Shdeps' foreground process group at lease acquisition,
        // so a negative kill stops the complete original job (including
        // pipeline siblings) without touching the parent shell's group.
        let stop_signal = if unsafe { libc::getsid(0) } == own_pid {
            libc::SIGSTOP
        } else {
            child_signal
        };
        if unsafe { libc::kill(-self.previous_group, stop_signal) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if received_signal().is_some() {
            // A termination signal may have remained pending while Shdeps was
            // stopped. Keep the terminal with Shdeps and let the supervision
            // loop run its normal CONT, TERM, bounded-KILL teardown instead
            // of unwinding into Drop's emergency KILL-only path.
            return Ok(());
        }
        // A shell resumes Shdeps in the foreground. Revalidate that handoff
        // before returning the terminal to the still-stopped child group.
        if unsafe { libc::tcgetpgrp(self.tty.as_raw_fd()) } != self.previous_group {
            return Err(std::io::Error::other(
                "terminal foreground changed while shdeps was stopped",
            ));
        }
        set_terminal_group(self.tty.as_raw_fd(), self.child_group)?;
        signal_group(self.child_group as u32, libc::SIGCONT)?;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TerminalForeground {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
fn suspend_background_job(child_signal: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY)
        .open("/dev/tty")?;
    // SAFETY: these calls only inspect the current process and this live TTY.
    let own_group = unsafe { libc::getpgrp() };
    let foreground = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    if own_group <= 0 || foreground <= 0 || foreground == own_group {
        return Err(std::io::Error::other(
            "terminal child stopped without a background caller job",
        ));
    }
    let own_pid = std::process::id() as i32;
    // Preserve normal shell job-control semantics. A private session has no
    // supervising shell to continue an orphaned group, so use SIGSTOP there.
    let stop_signal = if unsafe { libc::getsid(0) } == own_pid {
        libc::SIGSTOP
    } else {
        child_signal
    };
    // SAFETY: own_group is the current process group. Stopping it suspends the
    // complete invocation/pipeline without signaling the foreground shell.
    if unsafe { libc::kill(-own_group, stop_signal) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn set_terminal_group(fd: i32, group: i32) -> std::io::Result<()> {
    // tcsetpgrp from the temporarily-background Shdeps group would otherwise
    // raise SIGTTOU during restoration. Block it only in this calling thread
    // for the duration of the async-signal-safe ioctl.
    // SAFETY: all masks are initialized and remain live across both calls.
    unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        libc::sigaddset(&mut block, libc::SIGTTOU);
        let blocked = libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut previous);
        if blocked != 0 {
            return Err(std::io::Error::from_raw_os_error(blocked));
        }
        let result = libc::tcsetpgrp(fd, group);
        let saved_error = std::io::Error::last_os_error();
        let restored = libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result != 0 {
            return Err(saved_error);
        }
        if restored != 0 {
            return Err(std::io::Error::from_raw_os_error(restored));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum ObservedTermination {
    Exited(i32),
    Signaled(i32),
    Stopped(i32),
    Other,
}

fn record_observed_child_signal(termination: Option<ObservedTermination>, foreground: bool) {
    if !SIGNALS_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let signal = foreground
        .then(|| observed_handled_signal(termination))
        .flatten();
    if let Some(signal) = signal {
        record_signal(signal);
    }
}

fn observed_handled_signal(termination: Option<ObservedTermination>) -> Option<i32> {
    match termination {
        Some(ObservedTermination::Signaled(signal)) if handled_signal(signal) => Some(signal),
        Some(ObservedTermination::Exited(code)) => code
            .checked_sub(128)
            .filter(|signal| handled_signal(*signal)),
        _ => None,
    }
}

#[cfg(unix)]
fn handled_signal(signal: i32) -> bool {
    HANDLED_SIGNALS.contains(&signal)
}

#[cfg(not(unix))]
fn handled_signal(_signal: i32) -> bool {
    false
}

#[cfg(unix)]
fn record_child_signal(status: &ExitStatus, foreground: bool) {
    use std::os::unix::process::ExitStatusExt as _;
    let termination = status
        .signal()
        .map(ObservedTermination::Signaled)
        .or_else(|| status.code().map(ObservedTermination::Exited));
    record_observed_child_signal(termination, foreground);
}

#[cfg(not(unix))]
fn record_child_signal(_status: &ExitStatus, _foreground: bool) {}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.cleanup_complete {
            #[cfg(unix)]
            let _ = self.release_foreground();
            return;
        }
        #[cfg(unix)]
        if self.child.is_none() {
            if received_signal().is_some() {
                let cleanup = stop_reaped_boundary(
                    &mut self.boundary,
                    TERMINATE_SIGNAL,
                    self.foreground.as_ref(),
                );
                let foreground_error = self.release_foreground().err();
                if let Some(error) = foreground_error {
                    record_cleanup_error(&error);
                }
                if let Err(error) = cleanup {
                    record_cleanup_error(&error);
                }
            } else {
                remember_members_for_reap(&self.boundary);
                #[cfg(any(target_os = "linux", target_os = "android"))]
                if let Err(error) = reap_pending_members() {
                    record_cleanup_error(&error);
                }
                let _ = self.release_foreground();
            }
            return;
        }
        let mut release_child = false;
        if let Some(child) = &mut self.child {
            let mut leader_reaped = false;
            #[cfg(unix)]
            let stopped = stop_boundary(
                child,
                &mut self.boundary,
                KILL_SIGNAL,
                &mut leader_reaped,
                self.foreground.as_ref(),
            );
            #[cfg(not(unix))]
            let stopped = stop_boundary(child, self.isolation, KILL_SIGNAL, &mut leader_reaped);
            if stopped.is_err() {
                if let Err(error) = &stopped {
                    record_cleanup_error(error);
                }
                if !leader_reaped {
                    let _ = child.kill();
                    #[cfg(unix)]
                    let _ = wait_leader(child, &mut self.boundary);
                    #[cfg(not(unix))]
                    let _ = child.wait();
                }
            }
            release_child = stopped.is_ok() || leader_reaped;
        }
        if release_child {
            self.child.take();
        }
        #[cfg(unix)]
        if let Err(error) = self.release_foreground() {
            record_cleanup_error(&error);
        }
    }
}

// Observes exit without reaping on Unix, preserving the owned PID identity.
fn observe_exit(child: &mut Child) -> std::io::Result<Option<ObservedTermination>> {
    #[cfg(unix)]
    {
        // SAFETY: waitid initializes only the local siginfo, targets a retained
        // child PID, and WNOWAIT preserves the session identity for teardown.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
            ) != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error);
            }
            if info.si_pid() == 0 {
                return Ok(None);
            }
            let termination = match info.si_code {
                libc::CLD_EXITED => ObservedTermination::Exited(info.si_status()),
                libc::CLD_KILLED | libc::CLD_DUMPED => {
                    ObservedTermination::Signaled(info.si_status())
                }
                libc::CLD_STOPPED => ObservedTermination::Stopped(info.si_status()),
                _ => ObservedTermination::Other,
            };
            Ok(Some(termination))
        }
    }
    #[cfg(not(unix))]
    child.try_wait().map(|status| {
        status.map(|status| {
            status
                .code()
                .map_or(ObservedTermination::Other, ObservedTermination::Exited)
        })
    })
}

#[cfg(unix)]
fn observe_stopped(pid: u32) -> std::io::Result<Option<i32>> {
    // SAFETY: waitid observes one retained child and WNOWAIT preserves the
    // event until the owner decides whether it is a startup race or Ctrl-Z.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.si_pid() != 0 && info.si_code == libc::CLD_STOPPED).then(|| info.si_status()))
    }
}

#[cfg(unix)]
fn consume_stopped(pid: u32) -> std::io::Result<()> {
    // SAFETY: this consumes only a stop notification for the retained child;
    // WEXITED is absent, so an exit status cannot be reaped accidentally.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(libc::P_PID, pid, &mut info, libc::WSTOPPED | libc::WNOHANG) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// Whether one grace-loop empty observation advances the two-proof counter.
// Polls sharing a cached scan are not independent evidence of a stable
// empty set: two polls 20ms apart can read the same scan and falsely
// prove emptiness from a single enumeration. Only count an empty when at
// least `spacing` (the cache TTL) has passed since the last counted one,
// which guarantees a fresh enumeration happened between the two proofs.
// A zero spacing counts every empty; Linux passes zero because its
// per-member revalidation is already an independent observation.
#[cfg(unix)]
fn grace_empty_counts(now: Instant, last_counted: &mut Option<Instant>, spacing: Duration) -> bool {
    if last_counted.is_none_or(|at| now.saturating_duration_since(at) >= spacing) {
        *last_counted = Some(now);
        true
    } else {
        false
    }
}

// Dispatches teardown through the boundary retained from spawn. Keeping its
// identity set alive while the leader runs is what lets cancellation reach a
// descendant after that descendant changes group/session and is reparented.
#[cfg(unix)]
fn stop_boundary(
    child: &mut Child,
    boundary: &mut Boundary,
    first_signal: i32,
    leader_reaped: &mut bool,
    foreground: Option<&TerminalForeground>,
) -> std::io::Result<ExitStatus> {
    // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
    #[cfg(test)]
    let _diag = DiagScope::enter("stop_boundary");
    // Capture descendants once more before terminating the leader. Previously
    // observed identities remain owned even if PPID/PGID/SID has since changed.
    let discovery_deadline = Instant::now() + CLEANUP_SNAPSHOT_BUDGET;
    let mut first_error = None;
    let initial_observation = boundary.observe(discovery_deadline);
    if initial_observation.is_none() {
        let suffix = snapshot_failure_suffix();
        first_error = Some(std::io::Error::other(format!(
            "could not snapshot owned subprocesses during cancellation{suffix}"
        )));
    }
    retain_terminal_restore(&mut first_error, foreground, boundary);
    retain_first_error(&mut first_error, boundary.signal_stopped());
    let (delivery, graceful_deadline) = delivery_then_deadline(GRACE, || {
        boundary.signal_all(first_signal, initial_observation.is_some())
    });
    retain_first_error(&mut first_error, delivery);
    let mut consecutive_empty = 0;
    let mut grace_empty_at: Option<Instant> = None;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let exact_grace_delivery = exact_descendant_authority_available();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let mut next_fresh_discovery = Instant::now();
    while Instant::now() < graceful_deadline {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let observed = {
            if exact_grace_delivery && Instant::now() >= next_fresh_discovery {
                // A catchable-signal handler can fork after the initial exact
                // delivery. Discover that child at a bounded 50ms cadence and
                // deliver exactly to its pidfd below. Fresh global snapshots
                // are coalesced when task child lists are unavailable.
                let _ = boundary.reconcile_fresh(graceful_deadline);
                next_fresh_discovery = Instant::now() + TRACK_POLL;
            } else {
                boundary.refresh_known(graceful_deadline);
            }
            // Old kernels and restricted Android runtimes without exact PID
            // authority intentionally do not replay a catchable group signal:
            // that would re-enter every unchanged handler. Their one initial
            // group delivery is followed by the bounded group KILL phase,
            // which still guarantees teardown but cannot promise graceful
            // delivery to a child created by the first signal handler.
            // Stable-empty proof remains deferred to the fresh frozen-set
            // reconciliation after the grace period.
            Some(false)
        };
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let observed = boundary
            .observe_grace_cached(graceful_deadline)
            .map(|empty| empty && observe_exit(child).ok().flatten().is_some());
        retain_terminal_restore(&mut first_error, foreground, boundary);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let empty_spacing = Duration::ZERO;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let empty_spacing = GRACE_SNAPSHOT_TTL;
        if observed == Some(true)
            && grace_empty_counts(Instant::now(), &mut grace_empty_at, empty_spacing)
        {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                break;
            }
        } else if observed != Some(true) {
            consecutive_empty = 0;
            grace_empty_at = None;
        }
        // A same-window empty holds the count: no new scan evidence arrived,
        // so the boundary repolls instead of proving from one enumeration.
        retain_first_error(&mut first_error, boundary.signal_new(first_signal));
        std::thread::sleep(POLL);
    }
    if consecutive_empty >= 2 {
        // Grace proved the boundary empty with two consecutive successful
        // observations (spaced across distinct scans on cached platforms),
        // the same proof strength the KILL branch below requires before it
        // drops retained errors. A transient snapshot/delivery failure from
        // an earlier poll is definitionally non-fatal to this outcome: a
        // still-live member would have defeated the proof. Drop retained
        // errors so a successful cleanup cannot report CLEANUP_FAILED;
        // errors latched after this point still fail the stop.
        first_error = None;
    }
    if consecutive_empty < 2 {
        // Freeze the attributed set before final discovery. Unlike catchable
        // TERM, SIGSTOP cannot run a handler that forks into the snapshot/KILL
        // window. Existing descendants remain visible through their retained
        // lineage even if they finish while their parent is stopped.
        freeze_boundary(boundary, foreground, &mut first_error);
        retain_terminal_restore(&mut first_error, foreground, boundary);
        let (delivery, kill_deadline) = delivery_then_deadline(KILL_VERIFY_GRACE, || {
            boundary.signal_all(libc::SIGKILL, false)
        });
        retain_first_error(&mut first_error, delivery);
        consecutive_empty = 0;
        while Instant::now() < kill_deadline {
            let observed = boundary
                .observe(kill_deadline)
                .map(|empty| empty && observe_exit(child).ok().flatten().is_some());
            retain_terminal_restore(&mut first_error, foreground, boundary);
            retain_first_error(&mut first_error, boundary.signal_new(libc::SIGKILL));
            if observed == Some(true) {
                consecutive_empty += 1;
                if consecutive_empty >= 2 {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
            std::thread::sleep(POLL);
        }
        if consecutive_empty < 2
            && !settle_killed_boundary(
                boundary,
                foreground,
                &mut first_error,
                |boundary, deadline| {
                    boundary
                        .observe(deadline)
                        .map(|empty| empty && observe_exit(child).ok().flatten().is_some())
                },
            )
        {
            let survivors = boundary
                .current
                .values()
                .filter(|process| process.live)
                .map(|process| process.pid)
                .collect::<Vec<_>>();
            return Err(first_error.unwrap_or_else(|| {
                std::io::Error::other(format!(
                    "owned subprocesses survived bounded SIGKILL cleanup: {survivors:?}"
                ))
            }));
        }
        // KILL-phase verification proved the boundary empty (either in the
        // loop above or via settle): every error retained before that proof
        // — a quiesce timeout, a transient snapshot/delivery failure — is
        // definitionally non-fatal to the outcome. Drop them so a successful
        // cleanup cannot report CLEANUP_FAILED; errors latched after this
        // point (leader reap, terminal restore, adopted-zombie reap) still
        // fail the stop. Soundness rests on verify_empty_observation (the
        // lifetime-lease veto) plus None-on-failed-snapshot: the proof below
        // required two consecutive successful empty observations, so a
        // still-broken observer cannot produce it.
        first_error = None;
    }
    let status = wait_leader(child, boundary)?;
    *leader_reaped = true;
    retain_terminal_restore(&mut first_error, foreground, boundary);
    reap_members(boundary)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    retain_first_error(
        &mut first_error,
        reap_unobserved_adopted_zombies(Instant::now() + GRACE),
    );
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(status)
}

#[cfg(unix)]
fn wait_leader(child: &mut Child, boundary: &mut Boundary) -> std::io::Result<ExitStatus> {
    let status = child.wait()?;
    boundary.release_leader();
    Ok(status)
}

#[cfg(unix)]
fn stop_reaped_boundary(
    boundary: &mut Boundary,
    first_signal: i32,
    foreground: Option<&TerminalForeground>,
) -> std::io::Result<()> {
    let discovery_deadline = Instant::now() + CLEANUP_SNAPSHOT_BUDGET;
    let mut first_error = None;
    let initial_observation = boundary.observe(discovery_deadline);
    if initial_observation.is_none() {
        first_error = Some(std::io::Error::other(
            "could not snapshot owned subprocesses after leader reaping",
        ));
    }
    retain_terminal_restore(&mut first_error, foreground, boundary);
    retain_first_error(&mut first_error, boundary.signal_stopped());
    let (delivery, graceful_deadline) = delivery_then_deadline(GRACE, || {
        boundary.signal_all(first_signal, initial_observation.is_some())
    });
    retain_first_error(&mut first_error, delivery);

    let mut consecutive_empty = 0;
    let mut grace_empty_at: Option<Instant> = None;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let exact_grace_delivery = exact_descendant_authority_available();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let mut next_fresh_discovery = Instant::now();
    while Instant::now() < graceful_deadline {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let observed = {
            if exact_grace_delivery && Instant::now() >= next_fresh_discovery {
                let _ = boundary.reconcile_fresh(graceful_deadline);
                next_fresh_discovery = Instant::now() + TRACK_POLL;
            } else {
                boundary.refresh_known(graceful_deadline);
            }
            Some(false)
        };
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let observed = boundary.observe_grace_cached(graceful_deadline);
        retain_terminal_restore(&mut first_error, foreground, boundary);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let empty_spacing = Duration::ZERO;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let empty_spacing = GRACE_SNAPSHOT_TTL;
        if observed == Some(true)
            && grace_empty_counts(Instant::now(), &mut grace_empty_at, empty_spacing)
        {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                break;
            }
        } else if observed != Some(true) {
            consecutive_empty = 0;
            grace_empty_at = None;
        }
        // A same-window empty holds the count: no new scan evidence arrived,
        // so the boundary repolls instead of proving from one enumeration.
        retain_first_error(&mut first_error, boundary.signal_new(first_signal));
        std::thread::sleep(POLL);
    }

    if consecutive_empty < 2 {
        freeze_boundary(boundary, foreground, &mut first_error);
        retain_terminal_restore(&mut first_error, foreground, boundary);
        let (delivery, kill_deadline) = delivery_then_deadline(KILL_VERIFY_GRACE, || {
            boundary.signal_all(libc::SIGKILL, false)
        });
        retain_first_error(&mut first_error, delivery);
        consecutive_empty = 0;
        while Instant::now() < kill_deadline {
            let observed = boundary.observe(kill_deadline);
            retain_terminal_restore(&mut first_error, foreground, boundary);
            retain_first_error(&mut first_error, boundary.signal_new(libc::SIGKILL));
            if observed == Some(true) {
                consecutive_empty += 1;
                if consecutive_empty >= 2 {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
            std::thread::sleep(POLL);
        }
        if consecutive_empty < 2
            && !settle_killed_boundary(
                boundary,
                foreground,
                &mut first_error,
                |boundary, deadline| boundary.observe(deadline),
            )
        {
            let survivors = boundary
                .current
                .values()
                .filter(|process| process.live)
                .map(|process| process.pid)
                .collect::<Vec<_>>();
            return Err(first_error.unwrap_or_else(|| {
                std::io::Error::other(format!(
                    "owned subprocesses survived bounded post-reap SIGKILL cleanup: {survivors:?}"
                ))
            }));
        }
        // KILL-phase verification proved the boundary empty: pre-proof
        // retained errors are definitionally non-fatal (see stop_boundary).
        // Post-proof latches below still fail the stop.
        first_error = None;
    }

    retain_terminal_restore(&mut first_error, foreground, boundary);
    reap_members(boundary)?;
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn retain_terminal_restore(
    first_error: &mut Option<std::io::Error>,
    foreground: Option<&TerminalForeground>,
    boundary: &Boundary,
) {
    if let Some(foreground) = foreground {
        retain_first_error(first_error, foreground.restore_if_owned(boundary));
    }
}

#[cfg(unix)]
fn freeze_boundary(
    boundary: &mut Boundary,
    foreground: Option<&TerminalForeground>,
    first_error: &mut Option<std::io::Error>,
) {
    let (delivery, deadline) = delivery_then_deadline(LEADER_EXIT_SNAPSHOT_BUDGET, || {
        boundary.signal_all(libc::SIGSTOP, false)
    });
    retain_first_error(first_error, delivery);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut frozen = false;
        while Instant::now() < deadline {
            if boundary.refresh_known(deadline).is_none() {
                break;
            }
            retain_terminal_restore(first_error, foreground, boundary);
            let all_known_stopped = boundary
                .current
                .values()
                .all(|process| !process.live || process.stopped);
            if !all_known_stopped {
                std::thread::sleep(POLL);
                continue;
            }

            // Once every retained process is stopped, none can fork across
            // the following fresh marker scan. If that scan discovers a child
            // created just before STOP took effect, stop it and repeat; an
            // unchanged all-stopped set closes the final fork window without
            // requiring a third whole-procfs pass in the common case.
            let before = boundary
                .current
                .values()
                .filter(|process| process.live)
                .map(|process| process.identity.clone())
                .collect::<BTreeSet<_>>();
            let observed = boundary.reconcile_fresh(deadline);
            retain_terminal_restore(first_error, foreground, boundary);
            let after = boundary
                .current
                .values()
                .filter(|process| process.live)
                .map(|process| process.identity.clone())
                .collect::<BTreeSet<_>>();
            let all_stopped = boundary
                .current
                .values()
                .all(|process| !process.live || process.stopped);
            if observed.is_some() && all_stopped && before == after {
                frozen = true;
                break;
            }
            retain_first_error(first_error, boundary.signal_all(libc::SIGSTOP, false));
            std::thread::sleep(POLL);
        }
        if !frozen && first_error.is_none() {
            *first_error = Some(std::io::Error::other(
                "could not quiesce owned subprocesses before SIGKILL",
            ));
        }
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        let mut previous = None;
        let mut consecutive_frozen = 0;
        let mut frozen = false;
        while Instant::now() < deadline {
            // A local non-empty observation is enough on the normal hot path, but
            // not here: a signal handler may already have forked a marker-owned
            // child into another session. Force a complete reconciliation, stop
            // every newly attributed identity, and require the frozen set to be
            // unchanged across two observations before entering the KILL phase.
            let observed = boundary.reconcile_fresh(deadline);
            retain_terminal_restore(first_error, foreground, boundary);
            retain_first_error(first_error, boundary.signal_all(libc::SIGSTOP, false));
            let identities = boundary
                .current
                .values()
                .filter(|process| process.live)
                .map(|process| process.identity.clone())
                .collect::<BTreeSet<_>>();
            let all_stopped = boundary
                .current
                .values()
                .all(|process| !process.live || process.stopped);
            if observed.is_some() && all_stopped {
                consecutive_frozen = if previous.as_ref() == Some(&identities) {
                    consecutive_frozen + 1
                } else {
                    1
                };
                previous = Some(identities);
            } else {
                consecutive_frozen = 0;
                previous = None;
            }
            if consecutive_frozen >= 2 {
                frozen = true;
                break;
            }
            std::thread::sleep(POLL);
        }
        if !frozen && first_error.is_none() {
            *first_error = Some(std::io::Error::other(
                "could not quiesce owned subprocesses before SIGKILL",
            ));
        }
    }
}

#[cfg(unix)]
fn delivery_then_deadline<T>(duration: Duration, deliver: impl FnOnce() -> T) -> (T, Instant) {
    let result = deliver();
    (result, Instant::now() + duration)
}

#[cfg(unix)]
fn final_kill_verification_deadline<T>(deliver: impl FnOnce() -> T) -> (T, Instant) {
    delivery_then_deadline(KILL_SETTLE_GRACE, deliver)
}

#[cfg(unix)]
fn settle_killed_boundary(
    boundary: &mut Boundary,
    foreground: Option<&TerminalForeground>,
    first_error: &mut Option<std::io::Error>,
    mut observe_empty: impl FnMut(&mut Boundary, Instant) -> Option<bool>,
) -> bool {
    // Refresh once after the discovery phase and deliver to any identity that
    // appeared at its edge. The settlement deadline starts only after that
    // last delivery, so scheduling latency cannot consume the proof window.
    let final_discovery_deadline = Instant::now() + CLEANUP_SNAPSHOT_BUDGET;
    let observed = observe_empty(boundary, final_discovery_deadline);
    retain_terminal_restore(first_error, foreground, boundary);
    retain_first_error(first_error, boundary.signal_stopped());
    let (delivery, verification_deadline) =
        final_kill_verification_deadline(|| boundary.signal_new(libc::SIGKILL));
    retain_first_error(first_error, delivery);

    let mut consecutive_empty = usize::from(observed == Some(true));
    while consecutive_empty < 2 && Instant::now() < verification_deadline {
        std::thread::sleep(POLL);
        let observed = observe_empty(boundary, verification_deadline);
        retain_terminal_restore(first_error, foreground, boundary);
        if observed == Some(true) {
            consecutive_empty += 1;
        } else {
            consecutive_empty = 0;
        }
    }
    consecutive_empty >= 2
}

#[cfg(unix)]
fn retain_first_error<T>(slot: &mut Option<std::io::Error>, result: std::io::Result<T>) {
    if let Err(error) = result {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[cfg(not(unix))]
fn stop_boundary(
    child: &mut Child,
    isolation: Isolation,
    first_signal: i32,
    leader_reaped: &mut bool,
) -> std::io::Result<ExitStatus> {
    let _ = (isolation, first_signal);
    let _ = child.kill();
    let status = child.wait()?;
    *leader_reaped = true;
    Ok(status)
}

#[cfg(unix)]
struct Boundary {
    leader: u32,
    leader_retained: bool,
    isolation: Isolation,
    marker: BoundaryMarker,
    lifetime_reader: Option<std::os::unix::net::UnixStream>,
    members: BTreeMap<u32, ProcessIdentity>,
    current: BTreeMap<u32, ProcessInfo>,
    signaled: BTreeSet<ProcessIdentity>,
    phase_group_delivered: bool,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn unregister_active_leader(leader: u32) {
    let Some(leaders) = ACTIVE_BOUNDARY_LEADERS.get() else {
        return;
    };
    let mut leaders = leaders.lock().unwrap_or_else(|error| error.into_inner());
    let Some(registrations) = leaders.get_mut(&leader) else {
        return;
    };
    *registrations -= 1;
    if *registrations == 0 {
        leaders.remove(&leader);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn register_active_leader(leader: u32) {
    *ACTIVE_BOUNDARY_LEADERS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(leader)
        .or_default() += 1;
}

#[cfg(unix)]
impl Boundary {
    // Starts an empty process-set observation for one retained leader.
    fn new(leader: u32, isolation: Isolation, marker: BoundaryMarker) -> Self {
        let BoundaryMarker {
            token,
            lifetime_reader,
            lifetime_writer,
        } = marker;
        // `OwnedChild::new` runs only after spawn, so closing the parent's
        // writer here leaves EOF controlled exclusively by the child tree.
        drop(lifetime_writer);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        register_active_leader(leader);
        Self {
            leader,
            leader_retained: true,
            isolation,
            marker: BoundaryMarker::without_lifetime(token),
            lifetime_reader,
            members: BTreeMap::new(),
            current: BTreeMap::new(),
            signaled: BTreeSet::new(),
            phase_group_delivered: false,
        }
    }

    // Releases every numeric authority that depended on the leader still
    // reserving its PID. Descendant identities and the inherited marker remain
    // available for exact attribution after the leader has been reaped.
    fn release_leader(&mut self) {
        if !self.leader_retained {
            return;
        }
        self.leader_retained = false;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unregister_active_leader(self.leader);
        if let Some(identity) = self.members.remove(&self.leader) {
            self.unregister_identity(&identity);
        }
        self.current.remove(&self.leader);
    }

    // Retains descendant identities while the leader is still running. Linux
    // and Android follow only this boundary's task-child lists and adopted
    // children on the hot path. A strict shared whole-system snapshot is only
    // a fallback when local procfs traversal cannot prove completeness. Other
    // Unix platforms use the bounded portable snapshot.
    fn track(&mut self, deadline: Instant) -> Option<()> {
        // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
        #[cfg(test)]
        let _diag = DiagScope::enter("track");
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let (processes, locally_owned) = linux_local_or_full_snapshot(
                || linux_owned_process_snapshot(self, deadline),
                || cached_linux_process_snapshot(deadline),
            )?;
            if locally_owned {
                self.retain_owned_rows(&processes);
            }
            self.observe_processes(&processes).ok()?;
            Some(())
        }

        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            let processes = cached_portable_process_snapshot(deadline)?;
            self.observe_processes(&processes).ok()?;
            Some(())
        }
    }

    // Records matching topology and transitive PPID lineage. Retained
    // identities remain owned after reparenting or a process-group change.
    fn observe(&mut self, deadline: Instant) -> Option<bool> {
        // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
        #[cfg(test)]
        let _diag = DiagScope::enter("observe");
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            self.reconcile_fresh(deadline)
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            self.reconcile_fresh(deadline)
        }
    }

    // Leader exit needs a fresh attribution pass, but it does not yet need a
    // whole process-table topology proof. The private inherited marker finds
    // descendants that escaped both group and session without making every
    // normally completing child rescan all procfs stat records. Cancellation
    // and stable-empty cleanup continue to use `reconcile_fresh` below.
    // Portable platforms observe leader exit through the periodic `track`
    // path instead, so this boundary stays Linux-only.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn observe_leader_exit(&mut self, deadline: Instant) -> std::io::Result<bool> {
        let lifetime_has_holders = self.lifetime_has_holders()?;
        let processes = {
            if lifetime_has_holders == Some(false) {
                // EOF is a kernel observation made after the leader exited:
                // every process that inherited the private writer has either
                // exited or deliberately closed it. Refresh exact identities
                // locally when task child lists exist. Kernels that omit that
                // optional interface require one bounded full snapshot.
                let requested_at = Instant::now();
                let (processes, locally_owned) = linux_local_or_full_snapshot(
                    || linux_owned_process_snapshot(self, deadline),
                    || registered_linux_process_snapshot(requested_at, deadline),
                )
                .ok_or_else(|| {
                    std::io::Error::other("could not snapshot owned subprocesses after leader exit")
                })?;
                if locally_owned {
                    self.retain_owned_rows(&processes);
                    // Local traversal cannot see an escape the kernel
                    // reparented to this supervisor. Merge the detached
                    // (different-session) candidates before the
                    // unmarked-adoptee policy runs; only rows the policy
                    // attributes may join the retained membership.
                    let merged = linux_local_snapshot_with_supervisor_children(
                        (*processes).clone(),
                        deadline,
                        linux_supervisor_children,
                        linux_process_info_checked,
                    )?;
                    return self
                        .observe_processes_with_unmarked_adoptees(&merged, deadline, true)
                        .and_then(|empty| self.verify_empty_observation(empty, deadline));
                }
                return self
                    .observe_processes_with_unmarked_adoptees(&processes, deadline, true)
                    .and_then(|empty| self.verify_empty_observation(empty, deadline));
            } else {
                linux_boundary_marker_snapshot(self, deadline)?
            }
        };
        let empty = self.observe_processes(&processes)?;
        self.verify_empty_observation(empty, deadline)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn observe_after_leader_reap(&mut self, deadline: Instant) -> std::io::Result<bool> {
        if descendant_adoption_enabled()?
            && !supervisor_has_children()?
            && self.lifetime_has_holders()? == Some(false)
        {
            // With subreaper adoption enabled, every surviving descendant of
            // the reaped leader is now below a direct child of this process.
            // ECHILD is therefore a kernel-backed empty proof. A live child,
            // another active boundary, or an unexpectedly retained lifetime
            // descriptor takes the strict registered snapshot below.
            self.current.clear();
            return Ok(true);
        }

        let requested_at = Instant::now();
        let processes =
            registered_linux_process_snapshot(requested_at, deadline).ok_or_else(|| {
                std::io::Error::other("could not snapshot owned subprocesses after leader reaping")
            })?;
        let empty = self.observe_processes_with_unmarked_adoptees(&processes, deadline, true)?;
        self.verify_empty_observation(empty, deadline)
    }

    fn lifetime_has_holders(&mut self) -> std::io::Result<Option<bool>> {
        use std::io::Read as _;

        let Some(reader) = &mut self.lifetime_reader else {
            return Ok(None);
        };
        let mut byte = [0_u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => return Ok(Some(false)),
                Ok(_) => return Ok(Some(true)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(Some(true));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    // An otherwise empty process-table view is not a cleanup proof while the
    // private child-tree descriptor is still open. That state means a live
    // descendant inherited the lease but is no longer attributable through
    // its marker or retained topology. Fail closed rather than acknowledging
    // cancellation while that process can still mutate state.
    //
    // A concurrent spawn can fork while this boundary's writer is still open
    // in the parent; until that child execs, CLOEXEC cannot release the
    // inherited lease, and gated fixture children linger pre-exec behind
    // release sockets. An open lease with an empty snapshot is therefore
    // inconclusive while spawns are in flight: settle the captured cohort
    // (bounded by the caller deadline; later launches cannot have inherited
    // a writer this boundary already dropped) and re-read once. A lease
    // that is still open afterwards is a genuine leak: fail closed.
    fn verify_empty_observation(
        &mut self,
        empty: bool,
        deadline: Instant,
    ) -> std::io::Result<bool> {
        if !empty {
            return Ok(false);
        }
        match self.lifetime_has_holders()? {
            Some(false) | None => Ok(true),
            Some(true) => {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    if let Some(registry) = SPAWN_REGISTRATIONS.get() {
                        let _ = registry.wait_for_snapshot_registrations(deadline);
                    }
                    match self.lifetime_has_holders()? {
                        Some(false) | None => Ok(true),
                        Some(true) => Err(Self::open_lease_error()),
                    }
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                {
                    // Portable platforms have no spawn registry; their only
                    // gated fixture releases on signal acknowledgement
                    // (microseconds), so there is no lingering cohort to
                    // settle. Keep the immediate fail-closed.
                    let _ = deadline;
                    Err(Self::open_lease_error())
                }
            }
        }
    }

    fn open_lease_error() -> std::io::Error {
        std::io::Error::other(
            "owned subprocess ownership descriptor remains open after process discovery",
        )
    }

    // Cleanup proof boundaries must be based on a process-table view whose
    // enumeration started after the preceding signal or empty observation.
    // Linux performs a boundary-specific marker scan. Portable platforms may
    // coalesce concurrent whole-system scans established before enumeration
    // starts, while older hot-path cache entries are rejected.
    fn reconcile_fresh(&mut self, deadline: Instant) -> Option<bool> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            match linux_owned_process_snapshot(self, deadline) {
                Ok(processes) => {
                    self.retain_owned_rows(&processes);
                    self.observe_processes(&processes).ok()?;
                    let processes = linux_boundary_marker_snapshot(self, deadline).ok()?;
                    let empty = self.observe_processes(&processes).ok()?;
                    self.verify_empty_observation(empty, deadline).ok()
                }
                Err(_) => {
                    // CONFIG_PROC_CHILDREN is optional. When local traversal
                    // cannot prove completeness, use a fresh whole-process
                    // snapshot at this cancellation boundary rather than
                    // treating the missing interface as an empty child set.
                    let requested_at = Instant::now();
                    let processes = registered_linux_process_snapshot(requested_at, deadline)?;
                    let empty = self
                        .observe_processes_with_unmarked_adoptees(&processes, deadline, true)
                        .ok()?;
                    self.verify_empty_observation(empty, deadline).ok()
                }
            }
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let processes = fresh_process_snapshot(Instant::now(), deadline)?;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let empty = self.observe_processes(&processes).ok()?;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        self.verify_empty_observation(empty, deadline).ok()
    }

    // Grace-loop polling on portable platforms. Identical membership
    // reconciliation to reconcile_fresh, but concurrent boundaries share
    // whole-system scans at GRACE_SNAPSHOT_TTL instead of each spawning
    // `ps` per poll. Callers must space consecutive-empty proofs by the
    // same TTL (grace_empty_counts): polls sharing one cached scan are
    // not independent evidence of a stable empty set.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn observe_grace_cached(&mut self, deadline: Instant) -> Option<bool> {
        // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
        #[cfg(test)]
        let _diag = DiagScope::enter("observe");
        let now = Instant::now();
        let processes = PORTABLE_SNAPSHOT_CACHE
            .get_or_init(SnapshotCache::default)
            .get_or_load(now, GRACE_SNAPSHOT_TTL, deadline, process_snapshot)?;
        let empty = self.observe_processes(&processes).ok()?;
        self.verify_empty_observation(empty, deadline).ok()
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn retain_owned_rows(&mut self, processes: &[ProcessInfo]) {
        for process in processes {
            self.retain_identity(process.identity.clone());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn refresh_known(&mut self, deadline: Instant) -> Option<()> {
        match linux_owned_process_snapshot(self, deadline) {
            Ok(processes) => {
                self.retain_owned_rows(&processes);
                self.observe_processes(&processes).ok()?;
            }
            Err(_) => {
                // Cached process-table rows are useful for periodic discovery
                // but can predate a just-delivered STOP. Revalidate every
                // retained identity directly here so stale rows cannot erase
                // an owned member or hide its stopped state. Fresh global
                // reconciliation remains reserved for the proof boundary.
                let identities = self.members.values().cloned().collect::<Vec<_>>();
                self.current.clear();
                for identity in identities {
                    check_snapshot_deadline(deadline).ok()?;
                    let process = linux_process_info_checked(identity.pid).ok().flatten();
                    if let Some(process) = process.filter(|row| row.identity == identity) {
                        self.current.insert(process.pid, process);
                    }
                }
            }
        }
        Some(())
    }

    fn retain_identity(&mut self, identity: ProcessIdentity) {
        if let Some(replaced) = self.members.insert(identity.pid, identity.clone()) {
            if replaced != identity {
                self.unregister_identity(&replaced);
            }
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(token) = self.marker.token.as_deref() {
            let mut active = ACTIVE_BOUNDARY_IDENTITIES
                .get_or_init(|| Mutex::new(BTreeMap::new()))
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            active.entry(identity).or_default().insert(token.to_owned());
        }
    }

    fn unregister_identity(&self, identity: &ProcessIdentity) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let (Some(active), Some(token)) = (
            ACTIVE_BOUNDARY_IDENTITIES.get(),
            self.marker.token.as_deref(),
        ) {
            let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(tokens) = active.get_mut(identity) {
                tokens.remove(token);
                if tokens.is_empty() {
                    active.remove(identity);
                }
            }
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let _ = identity;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn registered_marker_disposition(
        &self,
        identity: &ProcessIdentity,
        deadline: Instant,
    ) -> std::io::Result<Option<bool>> {
        let active = lock_until(
            ACTIVE_BOUNDARY_IDENTITIES.get_or_init(|| Mutex::new(BTreeMap::new())),
            deadline,
        )
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "active boundary registry deadline expired",
            )
        })?;
        let Some(tokens) = active.get(identity) else {
            return Ok(None);
        };
        let own = self
            .marker
            .token
            .as_ref()
            .is_some_and(|token| tokens.contains(token));
        Ok(Some(own))
    }

    fn observe_processes(&mut self, processes: &[ProcessInfo]) -> std::io::Result<bool> {
        self.observe_processes_impl(processes, None)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn observe_processes_with_unmarked_adoptees(
        &mut self,
        processes: &[ProcessInfo],
        deadline: Instant,
        fail_on_ambiguous: bool,
    ) -> std::io::Result<bool> {
        self.observe_processes_impl(processes, Some((deadline, fail_on_ambiguous)))
    }

    #[cfg_attr(
        all(unix, not(any(target_os = "linux", target_os = "android"))),
        allow(unused_variables)
    )]
    fn observe_processes_impl(
        &mut self,
        processes: &[ProcessInfo],
        unmarked_adoptee_policy: Option<(Instant, bool)>,
    ) -> std::io::Result<bool> {
        let by_pid = processes
            .iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut owned = if self.leader_retained {
            BTreeSet::from([self.leader])
        } else {
            BTreeSet::new()
        };

        for (&pid, identity) in &self.members {
            if by_pid
                .get(&pid)
                .is_some_and(|process| process.identity == *identity)
            {
                owned.insert(pid);
            }
        }

        for process in by_pid.values() {
            let mut normally_owned =
                owned.contains(&process.pid) || self.contains_topology(process);
            let marker_disposition = if normally_owned {
                BoundaryMarkerDisposition::Different
            } else {
                self.adopted_marker_disposition(process)?
            };
            normally_owned |= marker_disposition == BoundaryMarkerDisposition::Matches;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let fallback_owned = if normally_owned {
                false
            } else if marker_disposition == BoundaryMarkerDisposition::Unknown {
                if let Some((deadline, fail_on_ambiguous)) = unmarked_adoptee_policy {
                    self.contains_unmarked_adoptee(process, &by_pid, deadline, fail_on_ambiguous)?
                } else {
                    false
                }
            } else {
                false
            };
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
            let fallback_owned = false;
            if normally_owned || fallback_owned {
                owned.insert(process.pid);
            }
        }

        // Closing this fixed point discovers grandchildren regardless of the
        // order in which `ps` or procfs returned their rows.
        loop {
            let before = owned.len();
            for process in by_pid.values() {
                if owned.contains(&process.ppid) {
                    owned.insert(process.pid);
                }
            }
            if owned.len() == before {
                break;
            }
        }

        self.current.clear();
        for pid in owned {
            if let Some(process) = by_pid.get(&pid) {
                self.retain_identity(process.identity.clone());
                self.current.insert(pid, (*process).clone());
            }
        }
        Ok(self.current.values().all(|process| !process.live))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn contains_unmarked_adoptee(
        &self,
        process: &ProcessInfo,
        by_pid: &BTreeMap<u32, &ProcessInfo>,
        deadline: Instant,
        fail_on_ambiguous: bool,
    ) -> std::io::Result<bool> {
        self.contains_unmarked_adoptee_with(
            process,
            by_pid,
            deadline,
            fail_on_ambiguous,
            linux_process_info_checked,
            |process| self.adopted_marker_disposition(process),
        )
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn contains_unmarked_adoptee_with(
        &self,
        process: &ProcessInfo,
        by_pid: &BTreeMap<u32, &ProcessInfo>,
        deadline: Instant,
        fail_on_ambiguous: bool,
        mut inspect: impl FnMut(u32) -> std::io::Result<Option<ProcessInfo>>,
        marker_disposition: impl Fn(&ProcessInfo) -> std::io::Result<BoundaryMarkerDisposition>,
    ) -> std::io::Result<bool> {
        if !process.live || process.ppid != std::process::id() {
            return Ok(false);
        }
        let Some(current) = inspect(process.pid)? else {
            return Ok(false);
        };
        if !current.live
            || current.ppid != std::process::id()
            || current.identity != process.identity
        {
            return Ok(false);
        }
        if self.leader_retained && !by_pid.get(&self.leader).is_some_and(|leader| !leader.live) {
            return Ok(false);
        }

        let leaders = lock_until(
            ACTIVE_BOUNDARY_LEADERS.get_or_init(|| Mutex::new(BTreeMap::new())),
            deadline,
        )
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "active subprocess registry deadline expired",
            )
        })?;
        if leaders.contains_key(&process.pid) {
            return Ok(false);
        }
        let active_count = leaders.values().copied().sum::<usize>();
        let sole_boundary = if self.leader_retained {
            active_count == 1 && leaders.contains_key(&self.leader)
        } else {
            active_count == 0
        };
        drop(leaders);

        let identities = lock_until(
            ACTIVE_BOUNDARY_IDENTITIES.get_or_init(|| Mutex::new(BTreeMap::new())),
            deadline,
        )
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "active boundary identity registry deadline expired",
            )
        })?;
        if let Some(tokens) = identities.get(&process.identity) {
            return Ok(self
                .marker
                .token
                .as_ref()
                .is_some_and(|token| tokens.contains(token)));
        }
        drop(identities);

        // The owner can reap and unregister a short-lived child after the
        // first identity check. Recheck after consulting both registries so a
        // stale live row cannot become either a false ownership claim or a
        // false cleanup-incomplete result.
        let Some(current) = inspect(process.pid)? else {
            return Ok(false);
        };
        if !current.live
            || current.ppid != std::process::id()
            || current.identity != process.identity
        {
            return Ok(false);
        }
        if sole_boundary {
            return Ok(true);
        }
        if fail_on_ambiguous && Instant::now() + AMBIGUOUS_ADOPTEE_SETTLE <= deadline {
            // Settle and re-verify once before failing closed. A concurrent
            // exec can present a markerless row that gains its marker
            // microseconds later; a short-lived unrelated child can exit
            // between the two inspections above. Either transient must not
            // poison a concurrent boundary's completion proof.
            std::thread::sleep(AMBIGUOUS_ADOPTEE_SETTLE);
            let settled = inspect(process.pid)?;
            let settled_live = settled.as_ref().is_some_and(|current| {
                current.live
                    && current.ppid == std::process::id()
                    && current.identity == process.identity
            });
            if !settled_live {
                // The row went stale across the settle: the short-lived
                // adoptee exited or its PID was reused. Either way there is
                // nothing left to attribute or clean up.
                return Ok(false);
            }
            let current = settled.expect("settled live row is present");
            match marker_disposition(&current)? {
                // The marker appeared after the first read: this process is
                // ours despite the earlier unknown row.
                BoundaryMarkerDisposition::Matches => return Ok(true),
                // Another worker's process, or a row that vanished between
                // the settle inspection and the marker lookup.
                BoundaryMarkerDisposition::Different | BoundaryMarkerDisposition::Gone => {
                    return Ok(false);
                }
                BoundaryMarkerDisposition::Unknown => {}
            }
            // A same-session direct child is indistinguishable from a plain
            // supervisor spawn: only setsid moves a process out of its
            // inherited session. Ignoring the ambiguous row matches the
            // leader-exit merge, which never surfaces same-session
            // children, so local and full observations agree. Detached
            // adoptees keep the escapee shape and still fail closed below.
            // SAFETY: getsid observes our own session without pointers.
            let own_sid = unsafe { libc::getsid(0) };
            if own_sid >= 0 && current.sid == own_sid as u32 {
                return Ok(false);
            }
        }
        // Once a descendant has deliberately closed both attribution
        // channels, its former parent is no longer observable after
        // reparenting. Claim it only when this is the sole active boundary;
        // assigning it while another boundary is active could signal that
        // worker's process. Neither normal completion nor cancellation may
        // acknowledge success while the unattributed live child remains.
        if fail_on_ambiguous {
            Err(std::io::Error::other(format!(
                "could not attribute adopted subprocess {} to one active boundary",
                process.pid
            )))
        } else {
            Ok(false)
        }
    }

    // Tests a process-table row against the topology created at spawn. Exact
    // children deliberately share the caller's topology and use lineage only.
    fn contains_topology(&self, process: &ProcessInfo) -> bool {
        if !self.leader_retained {
            return false;
        }
        match self.isolation {
            Isolation::ExactChild | Isolation::ParentSession => process.pgid == self.leader,
            Isolation::DetachedSession => process.sid == self.leader,
        }
    }

    // A descendant may deliberately leave both the root group and session and
    // then outlive its leader. The inherited boundary marker preserves safe
    // attribution after reparenting: Linux/Android descendants are adopted by
    // this process's subreaper; other Unix kernels hand them to init. Only an
    // exact marker match can extend ownership beyond ordinary topology.
    fn adopted_marker_disposition(
        &self,
        process: &ProcessInfo,
    ) -> std::io::Result<BoundaryMarkerDisposition> {
        let Some(token) = self.marker.token.as_deref() else {
            return Ok(BoundaryMarkerDisposition::Unknown);
        };
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let adopted = process.ppid == std::process::id();
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let adopted = process.ppid == 1;

        if !adopted {
            return Ok(BoundaryMarkerDisposition::Different);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return match process_boundary_marker_disposition_checked(process.pid, token) {
            Ok(Some(disposition)) => Ok(disposition),
            // The snapshot row predates the failed lookup. ENOENT proves that
            // this exact row vanished; treating it as an unknown live adoptee
            // would let a short-lived unrelated child poison every concurrent
            // boundary's completion proof.
            Ok(None) => Ok(BoundaryMarkerDisposition::Gone),
            // A concurrent exec can make environ temporarily unreadable. Do
            // not let one direct adoptee poison every other boundary's normal
            // leader-exit observation. Strict cancellation reconciliation
            // separately classifies an unmarked direct adoptee and fails
            // closed when multiple active boundaries make ownership
            // ambiguous; an open lifetime lease also prevents an empty proof.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                Ok(BoundaryMarkerDisposition::Unknown)
            }
            Err(error) => Err(error),
        };
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        return Ok(if process_has_boundary_marker(process.pid, token) {
            BoundaryMarkerDisposition::Matches
        } else {
            BoundaryMarkerDisposition::Different
        });
    }

    // Revalidates a remembered PID immediately before delivery. The process
    // table can churn after a snapshot; cached PID/topology alone must never
    // authorize a signal to a reused or unrelated process.
    fn revalidated_process(&self, pid: u32) -> std::io::Result<Option<ProcessInfo>> {
        let Some(expected) = self.members.get(&pid) else {
            return Ok(None);
        };
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let current = linux_process_info_checked(pid)?;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let current = portable_process_info(pid, expected.clone());
        let Some(current) = current else {
            return Ok(None);
        };

        // Membership was admitted only from retained topology, transitive
        // lineage, or the private marker. Once admitted, the exact start-time
        // identity remains the authority even if the process later changes
        // group/session or clears its inherited environment.
        Ok((current.identity == *expected).then_some(current))
    }

    fn signal_process(&self, process: &ProcessInfo, signal: i32) -> std::io::Result<bool> {
        let Some(current) = self.revalidated_process(process.pid)? else {
            return Ok(false);
        };
        if !current.live {
            return Ok(false);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if linux_signal_authority(self.leader_retained, self.leader, &current)
                == LinuxSignalAuthority::RetainedGroup
            {
                return signal_group(self.leader, signal);
            }
            if !exact_descendant_authority_available() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "safe escaped-descendant delivery requires pidfd open, signal, and wait support",
                ));
            }
            let Some(pidfd) = stable_pidfd(open_pidfd(current.pid)?)? else {
                return Ok(false);
            };
            // Opening the pidfd pins whichever process owns this number now.
            // Revalidate again before using that handle so a reuse between the
            // first topology check and pidfd_open cannot redirect delivery.
            let Some(confirmed) = self.revalidated_process(current.pid)? else {
                return Ok(false);
            };
            if !confirmed.live || confirmed.identity != current.identity {
                return Ok(false);
            }
            // SIGSTOP is used only to close the final fork window before
            // SIGKILL. A live exact group leader plus its retained pidfd pins
            // that group number across this delivery, so freezing the group
            // also catches a child forked immediately before discovery.
            if signal == libc::SIGSTOP && confirmed.pid == confirmed.pgid {
                return signal_group(confirmed.pid, signal);
            }
            signal_pidfd(&pidfd, signal)
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            let Some(group) = self
                .leader_retained
                .then(|| portable_signal_group_for(self.leader, process, &current))
                .flatten()
            else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "safe portable delivery requires the retained original process group",
                ));
            };
            signal_group(group, signal)
        }
    }

    // A stopped child cannot run a graceful handler. Continue only identities
    // the latest process snapshot actually reports as stopped; SIGCONT is
    // otherwise observable to hooks and must not be sprayed unconditionally.
    fn signal_stopped(&self) -> std::io::Result<()> {
        let mut first_error = None;
        for process in self.current.values().filter(|process| process.stopped) {
            if process.pid == self.leader {
                retain_first_error(&mut first_error, signal_group(self.leader, libc::SIGCONT));
            } else {
                retain_first_error(
                    &mut first_error,
                    self.signal_process(process, libc::SIGCONT),
                );
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    // Gracefully signals newly discovered members without PID-only gaps.
    fn signal_new(&mut self, signal: i32) -> std::io::Result<()> {
        let pending = self
            .current
            .values()
            .filter(|process| {
                process.pid != self.leader
                    && process.live
                    && !self.signaled.contains(&process.identity)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut phase = SignalPhase {
            signaled: std::mem::take(&mut self.signaled),
            group_delivered: self.phase_group_delivered,
        };
        let result = deliver_new_signal_phase(
            self.leader_retained.then_some(self.leader),
            &pending,
            signal,
            &mut phase,
            |process| {
                self.revalidated_process(process.pid)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current.pgid == self.leader)
            },
            |process, signal| self.signal_exact_process(process, signal),
        );
        self.signaled = phase.signaled;
        self.phase_group_delivered = phase.group_delivered;
        result
    }

    // Delivers final escalation to every still-owned process identity.
    fn signal_all(&mut self, signal: i32, cohort_complete: bool) -> std::io::Result<()> {
        let current = self.current.values().cloned().collect::<Vec<_>>();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let prefer_exact_delivery = cohort_complete && exact_descendant_authority_available();
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let prefer_exact_delivery = {
            let _ = cohort_complete;
            false
        };
        let (phase, result) = deliver_initial_signal_phase(
            self.leader,
            self.leader_retained,
            &current,
            signal,
            prefer_exact_delivery,
            signal_group,
            |process| {
                self.revalidated_process(process.pid)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current.pgid == self.leader)
            },
            |process, signal| self.signal_exact_process(process, signal),
        );
        self.signaled = phase.signaled;
        self.phase_group_delivered = phase.group_delivered;
        result
    }

    fn signal_exact_process(&self, process: &ProcessInfo, signal: i32) -> std::io::Result<bool> {
        let Some(current) = self.revalidated_process(process.pid)? else {
            return Ok(false);
        };
        if !current.live {
            return Ok(false);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if !exact_descendant_authority_available() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "safe escaped-descendant delivery requires pidfd open, signal, and wait support",
                ));
            }
            let Some(pidfd) = stable_pidfd(open_pidfd(current.pid)?)? else {
                return Ok(false);
            };
            let Some(confirmed) = self.revalidated_process(current.pid)? else {
                return Ok(false);
            };
            if !confirmed.live || confirmed.identity != current.identity {
                return Ok(false);
            }
            if signal == libc::SIGSTOP && confirmed.pid == confirmed.pgid {
                return signal_group(confirmed.pid, signal);
            }
            signal_pidfd(&pidfd, signal)
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            let _ = signal;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "safe portable delivery cannot address an escaped process identity",
            ))
        }
    }
}

#[cfg(unix)]
impl Drop for Boundary {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.leader_retained {
            unregister_active_leader(self.leader);
            self.leader_retained = false;
        }
        let identities = self.members.values().cloned().collect::<Vec<_>>();
        for identity in identities {
            self.unregister_identity(&identity);
        }
    }
}

#[cfg(unix)]
struct SignalPhase {
    signaled: BTreeSet<ProcessIdentity>,
    group_delivered: bool,
}

#[cfg(unix)]
#[expect(
    clippy::too_many_arguments,
    reason = "the signal-delivery seam keeps each authority decision independently injectable"
)]
fn deliver_initial_signal_phase(
    leader: u32,
    leader_retained: bool,
    current: &[ProcessInfo],
    signal: i32,
    prefer_exact_delivery: bool,
    mut signal_retained_group: impl FnMut(u32, i32) -> std::io::Result<bool>,
    mut remains_in_retained_group: impl FnMut(&ProcessInfo) -> bool,
    mut signal_exact: impl FnMut(&ProcessInfo, i32) -> std::io::Result<bool>,
) -> (SignalPhase, std::io::Result<()>) {
    let mut phase = SignalPhase {
        signaled: BTreeSet::new(),
        group_delivered: false,
    };
    let mut first_error = None;
    // Exact kernel handles let the initial frozen cohort and every later
    // discovery receive a signal exactly once. A process-group broadcast can
    // reach a child that was absent from `current`; later treating that child
    // as new would deliver the same catchable signal twice. Retain group
    // delivery for portable/no-pidfd runtimes. A retained zombie leader needs
    // no delivery and must not force its still-live descendants back onto the
    // group path.
    let exact_initial_cohort = prefer_exact_delivery;
    if leader_retained && !exact_initial_cohort {
        match signal_retained_group(leader, signal) {
            Ok(delivered) => phase.group_delivered = delivered,
            Err(error) => first_error = Some(error),
        }
    }
    for process in current.iter().filter(|process| process.live) {
        if !exact_initial_cohort
            && leader_retained
            && phase.group_delivered
            && remains_in_retained_group(process)
        {
            phase.signaled.insert(process.identity.clone());
            continue;
        }
        match signal_exact(process, signal) {
            Ok(true) => {
                phase.signaled.insert(process.identity.clone());
            }
            Ok(false) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    let result = first_error.map_or(Ok(()), Err);
    (phase, result)
}

#[cfg(unix)]
fn deliver_new_signal_phase(
    retained_leader: Option<u32>,
    current: &[ProcessInfo],
    signal: i32,
    phase: &mut SignalPhase,
    mut remains_in_retained_group: impl FnMut(&ProcessInfo) -> bool,
    mut signal_exact: impl FnMut(&ProcessInfo, i32) -> std::io::Result<bool>,
) -> std::io::Result<()> {
    let mut first_error = None;
    for process in current {
        if !process.live || phase.signaled.contains(&process.identity) {
            continue;
        }
        if retained_leader
            .filter(|_| remains_in_retained_group(process))
            .is_some()
        {
            // Prefer exact delivery so a TERM handler can create a same-group
            // child without re-entering unchanged handlers. A no-pidfd runtime
            // has already delivered once to the retained group; it defers
            // post-delivery children to the bounded group KILL phase rather
            // than replaying a catchable signal to the initial cohort.
            match signal_exact(process, signal) {
                Ok(true) => {
                    phase.signaled.insert(process.identity.clone());
                }
                Ok(false) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::Unsupported && phase.group_delivered => {
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        } else {
            match signal_exact(process, signal) {
                Ok(true) => {
                    phase.signaled.insert(process.identity.clone());
                }
                Ok(false) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
fn portable_signal_group_for(
    retained_leader: u32,
    expected: &ProcessInfo,
    current: &ProcessInfo,
) -> Option<u32> {
    (expected.identity == current.identity && current.live && current.pgid == retained_leader)
        .then_some(retained_leader)
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    pgid: u32,
    sid: u32,
    live: bool,
    stopped: bool,
    identity: ProcessIdentity,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ProcessIdentity {
    pid: u32,
    start: Option<String>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundaryMarkerDisposition {
    Matches,
    Different,
    Unknown,
    // Only procfs lookups can prove the snapshot row vanished; portable
    // platforms never construct this disposition.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Gone,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxSignalAuthority {
    RetainedGroup,
    ExactProcess,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_signal_authority(
    leader_retained: bool,
    leader: u32,
    current: &ProcessInfo,
) -> LinuxSignalAuthority {
    if leader_retained && current.pgid == leader {
        LinuxSignalAuthority::RetainedGroup
    } else {
        LinuxSignalAuthority::ExactProcess
    }
}

#[cfg(unix)]
#[derive(Default)]
struct SnapshotCache {
    captured: Mutex<Option<(Instant, Arc<Vec<ProcessInfo>>)>>,
    loading: AtomicBool,
}

#[cfg(unix)]
impl SnapshotCache {
    fn get_or_load(
        &self,
        requested_at: Instant,
        max_age: Duration,
        deadline: Instant,
        load: impl FnOnce(Instant) -> Option<Vec<ProcessInfo>>,
    ) -> Option<Arc<Vec<ProcessInfo>>> {
        let mut load = Some(load);
        loop {
            let captured = lock_until(&self.captured, deadline)?;
            if let Some((captured_at, processes)) = &*captured {
                if *captured_at >= requested_at
                    || requested_at.saturating_duration_since(*captured_at) < max_age
                {
                    return Some(Arc::clone(processes));
                }
            }
            drop(captured);

            if self
                .loading
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _loading = SnapshotLoadingGuard(&self.loading);
                // Record when enumeration starts, not when it finishes. A
                // waiter may share this scan only when its proof boundary was
                // established before the scan began; completion time alone
                // cannot prove that the earlier process-table rows are fresh.
                let captured_at = Instant::now();
                let processes = Arc::new(load.take()?(deadline)?);
                let mut captured = lock_until(&self.captured, deadline)?;
                *captured = Some((captured_at, Arc::clone(&processes)));
                return Some(processes);
            }

            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(unix)]
struct SnapshotLoadingGuard<'a>(&'a AtomicBool);

#[cfg(unix)]
impl Drop for SnapshotLoadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(unix)]
fn lock_until<T>(mutex: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

#[cfg(unix)]
fn write_lock_until<T>(lock: &RwLock<T>, deadline: Instant) -> Option<RwLockWriteGuard<'_, T>> {
    loop {
        match lock.try_write() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

#[cfg(unix)]
fn spawn_registration_guard() -> std::io::Result<RwLockReadGuard<'static, ()>> {
    spawn_registration_guard_before(None, true)
}

// Teardown observability must work during teardown. The portable snapshot's
// `ps` helper is cleanup instrumentation, not new user work, so it takes the
// spawn-registration read lock without the cancellation refusal. Refusing it
// fails every cold-cache observation after a signal on platforms without
// procfs, which both slows teardown past its proof deadlines and latches
// CLEANUP_FAILED (misreporting 1 instead of 128+signal).
#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
fn spawn_registration_guard_unchecked_until(
    deadline: Instant,
) -> std::io::Result<RwLockReadGuard<'static, ()>> {
    spawn_registration_guard_before(Some(deadline), false)
}

#[cfg(unix)]
fn spawn_registration_guard_before(
    deadline: Option<Instant>,
    enforce_cancellation: bool,
) -> std::io::Result<RwLockReadGuard<'static, ()>> {
    loop {
        if enforce_cancellation {
            check()?;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "owned subprocess launch deadline expired",
            ));
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let reaper_waiting = ADOPTED_REAPER_WAITING.load(Ordering::Acquire);
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let reaper_waiting = false;
        if !reaper_waiting {
            match OWNED_CHILD_SPAWN.try_read() {
                Ok(guard) => {
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    let reaper_waiting = ADOPTED_REAPER_WAITING.load(Ordering::Acquire);
                    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
                    let reaper_waiting = false;
                    if !reaper_waiting {
                        return Ok(guard);
                    }
                    drop(guard);
                }
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    let guard = error.into_inner();
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    let reaper_waiting = ADOPTED_REAPER_WAITING.load(Ordering::Acquire);
                    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
                    let reaper_waiting = false;
                    if !reaper_waiting {
                        return Ok(guard);
                    }
                    drop(guard);
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
        }
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(1))
            .min(Duration::from_millis(1));
        if wait.is_zero() {
            continue;
        }
        std::thread::sleep(wait);
    }
}

#[cfg(unix)]
pub(crate) fn exclusive_spawn_guard(
    deadline: Instant,
) -> std::io::Result<RwLockWriteGuard<'static, ()>> {
    loop {
        check()?;
        let now = Instant::now();
        if let Some(guard) = write_lock_until(&OWNED_CHILD_SPAWN, deadline.min(now + POLL)) {
            return Ok(guard);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "owned subprocess launch quiescence deadline expired",
            ));
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn test_spawn_guard() -> std::io::Result<impl Drop> {
    spawn_registration_guard()
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) struct TestChildRegistration(u32);

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
impl Drop for TestChildRegistration {
    fn drop(&mut self) {
        unregister_active_leader(self.0);
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) fn test_child_registration(pid: u32) -> TestChildRegistration {
    register_active_leader(pid);
    TestChildRegistration(pid)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default)]
struct SpawnRegistrationState {
    next: u64,
    active: BTreeSet<u64>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default)]
struct SpawnRegistrationRegistry {
    state: Mutex<SpawnRegistrationState>,
    changed: Condvar,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SpawnRegistrationRegistry {
    fn begin(&'static self) -> SpawnRegistrationWindow {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.next = state.next.wrapping_add(1);
        let id = state.next;
        state.active.insert(id);
        SpawnRegistrationWindow { registry: self, id }
    }

    // Called after a process snapshot. Every spawn that could have appeared
    // in that snapshot has an id at or below the captured frontier. Wait only
    // for that finite cohort; later launches cannot be present in the already
    // completed snapshot and therefore do not delay its classification.
    fn wait_for_snapshot_registrations(&self, deadline: Instant) -> Option<()> {
        let mut state = lock_until(&self.state, deadline)?;
        let frontier = state.next;
        while state.active.range(..=frontier).next().is_some() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            if timeout.timed_out() && state.active.range(..=frontier).next().is_some() {
                return None;
            }
        }
        Some(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
struct SpawnRegistrationWindow {
    registry: &'static SpawnRegistrationRegistry,
    id: u64,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SpawnRegistrationWindow {
    fn begin() -> Self {
        SPAWN_REGISTRATIONS
            .get_or_init(SpawnRegistrationRegistry::default)
            .begin()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for SpawnRegistrationWindow {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active.remove(&self.id);
        drop(state);
        self.registry.changed.notify_all();
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) fn test_spawn_registration() -> impl Drop {
    SpawnRegistrationWindow::begin()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn cached_portable_process_snapshot(deadline: Instant) -> Option<Arc<Vec<ProcessInfo>>> {
    let now = Instant::now();
    PORTABLE_SNAPSHOT_CACHE
        .get_or_init(SnapshotCache::default)
        .get_or_load(now, PORTABLE_SNAPSHOT_TTL, deadline, process_snapshot)
}

#[cfg(unix)]
// Takes one bounded portable snapshot of PID, process group, session, and state.
fn process_snapshot(deadline: Instant) -> Option<Vec<ProcessInfo>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        linux_process_snapshot_checked(deadline).ok()
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        #[cfg(target_os = "android")]
        let mut command = Command::new("/system/bin/ps");
        #[cfg(not(target_os = "android"))]
        let mut command = Command::new("/bin/ps");
        #[cfg(target_os = "android")]
        command.args(["-A", "-o", "pid=,ppid=,stat="]);
        #[cfg(not(target_os = "android"))]
        command.args(["-A", "-o", "pid=,ppid=,stat=,lstart="]);
        let bytes = snapshot(command, deadline)?;
        let text = String::from_utf8(bytes).ok()?;
        parse_ps_processes(
            &text,
            || Instant::now() >= deadline,
            |pid| {
                #[cfg(target_vendor = "apple")]
                {
                    let process = darwin_process_info(pid)?;
                    if Instant::now() >= deadline {
                        return None;
                    }
                    return Some((process.pgid, process.sid, process.identity.start));
                }
                #[cfg(not(target_vendor = "apple"))]
                let pgid = process_group(pid)?;
                #[cfg(not(target_vendor = "apple"))]
                if Instant::now() >= deadline {
                    return None;
                }
                #[cfg(not(target_vendor = "apple"))]
                return Some((pgid, session_id(pid)?, None));
            },
        )
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_snapshot_checked(deadline: Instant) -> std::io::Result<Vec<ProcessInfo>> {
    let entries = std::fs::read_dir("/proc")?.map(|entry| {
        entry.map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
        })
    });
    collect_linux_process_snapshot_with(entries, deadline, linux_process_info_checked)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_boundary_marker_snapshot(
    boundary: &Boundary,
    deadline: Instant,
) -> std::io::Result<Vec<ProcessInfo>> {
    if boundary.marker.token.is_none() {
        return linux_process_snapshot_checked(deadline);
    }
    let entries = std::fs::read_dir("/proc")?.map(|entry| {
        entry.map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
        })
    });
    collect_linux_boundary_marker_snapshot_with(
        boundary,
        entries,
        deadline,
        linux_process_info_checked,
        process_has_boundary_marker_checked,
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn collect_linux_boundary_marker_snapshot_with(
    boundary: &Boundary,
    entries: impl IntoIterator<Item = std::io::Result<Option<u32>>>,
    deadline: Instant,
    mut inspect: impl FnMut(u32) -> std::io::Result<Option<ProcessInfo>>,
    mut has_marker: impl FnMut(u32, &str) -> std::io::Result<Option<bool>>,
) -> std::io::Result<Vec<ProcessInfo>> {
    let token = boundary
        .marker
        .token
        .as_deref()
        .ok_or_else(|| std::io::Error::other("boundary marker is unavailable"))?;
    // Collect the directory before opening any process-owned file. An error in
    // getdents invalidates the entire attribution pass rather than publishing
    // a prefix that could omit a marker-owned descendant.
    let pids = entries.into_iter().collect::<std::io::Result<Vec<_>>>()?;
    let mut processes = Vec::new();
    let mut unresolved_adoptees = Vec::new();
    for pid in pids {
        check_snapshot_deadline(deadline)?;
        let Some(pid) = pid else {
            continue;
        };
        let retained = (boundary.leader_retained && pid == boundary.leader)
            || boundary.members.contains_key(&pid);
        let process = if retained {
            inspect(pid)?
        } else {
            match has_marker(pid, token) {
                Ok(Some(true)) => {
                    // The marker and stat files are separate procfs lookups.
                    // Bracket a second marker read with matching process
                    // identities so PID reuse cannot splice an old marker
                    // onto a newly unrelated process generation.
                    let Some(before) = inspect(pid)? else {
                        continue;
                    };
                    if has_marker(pid, token)? != Some(true) {
                        None
                    } else {
                        let after = inspect(pid)?;
                        match after {
                            Some(after) if after.identity == before.identity => Some(after),
                            _ => None,
                        }
                    }
                }
                Ok(Some(false) | None) => None,
                // Android hides other apps' environ files behind permission
                // errors, and the stat read is denied the same way; those
                // entries cannot be owned descendants or adoptees, so skip
                // them rather than failing a snapshot complete for
                // everything owned.
                #[cfg(target_os = "android")]
                Err(error) if procfs_process_foreign(&error) => None,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    // Permission-denied environments are common for unrelated
                    // users. Inspect their public topology before excluding
                    // them: a member of the retained group/lineage is still
                    // ours, while an unidentifiable live adoptee must make the
                    // cleanup proof fail closed.
                    let candidate = inspect(pid)?;
                    match candidate {
                        Some(process)
                            if boundary.contains_topology(&process)
                                || boundary.members.contains_key(&process.ppid) =>
                        {
                            Some(process)
                        }
                        Some(process) if process.live && process.ppid == std::process::id() => {
                            let _ = error;
                            match boundary
                                .registered_marker_disposition(&process.identity, deadline)?
                            {
                                Some(true) => Some(process),
                                Some(false) => None,
                                None => {
                                    unresolved_adoptees.push(process);
                                    None
                                }
                            }
                        }
                        _ => None,
                    }
                }
                Err(error) => return Err(error),
            }
        };
        if let Some(process) = process {
            processes.push(process);
        }
    }
    // A concurrent exec can temporarily deny access to `/proc/PID/environ`.
    // Retry only live direct adoptees that could belong to this boundary. An
    // unrelated child will expose a different marker after exec completes; a
    // persistent ambiguity fails closed at the caller's deadline.
    while !unresolved_adoptees.is_empty() {
        check_snapshot_deadline(deadline).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "could not verify ownership of adopted processes {:?}",
                    unresolved_adoptees
                        .iter()
                        .map(|process| process.pid)
                        .collect::<Vec<_>>()
                ),
            )
        })?;
        let mut still_unresolved = Vec::new();
        for expected in unresolved_adoptees {
            check_snapshot_deadline(deadline)?;
            let Some(current) = inspect(expected.pid)? else {
                continue;
            };
            if current.identity != expected.identity || !current.live {
                continue;
            }
            match has_marker(current.pid, token) {
                Ok(Some(true)) => processes.push(current),
                Ok(Some(false) | None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    still_unresolved.push(current);
                }
                Err(error) => return Err(error),
            }
        }
        unresolved_adoptees = still_unresolved;
        if !unresolved_adoptees.is_empty() {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(POLL));
        }
    }
    Ok(processes)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn collect_linux_process_snapshot_with(
    entries: impl IntoIterator<Item = std::io::Result<Option<u32>>>,
    deadline: Instant,
    mut inspect: impl FnMut(u32) -> std::io::Result<Option<ProcessInfo>>,
) -> std::io::Result<Vec<ProcessInfo>> {
    // Finish enumerating the directory before opening a process file. An
    // iterator error must invalidate the whole snapshot rather than publish a
    // prefix that can be mistaken for a stable-empty process set.
    let entries = entries.into_iter().collect::<std::io::Result<Vec<_>>>()?;
    let mut processes = Vec::new();
    for pid in entries {
        let Some(pid) = pid else {
            continue;
        };
        check_snapshot_deadline(deadline)?;
        let process = match inspect(pid) {
            Ok(process) => process,
            // Android hides other apps' stat files behind permission
            // errors; those entries cannot be owned descendants, so skip
            // them rather than failing a snapshot that is complete for
            // everything owned.
            #[cfg(target_os = "android")]
            Err(error) if procfs_process_foreign(&error) => continue,
            Err(error) => return Err(error),
        };
        if let Some(process) = process {
            processes.push(process);
        }
    }
    Ok(processes)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_info_checked(pid: u32) -> std::io::Result<Option<ProcessInfo>> {
    linux_process_info_at_checked(pid, &std::path::PathBuf::from(format!("/proc/{pid}")))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_owned_process_snapshot(
    boundary: &Boundary,
    deadline: Instant,
) -> std::io::Result<Vec<ProcessInfo>> {
    match linux_task_children_interface_at(std::path::Path::new("/proc"), std::process::id())? {
        LinuxTaskChildrenInterface::Available => {}
        LinuxTaskChildrenInterface::Unavailable => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "procfs task child lists are unavailable",
            ));
        }
        LinuxTaskChildrenInterface::ProcessGone => {
            return Err(std::io::Error::other(
                "current process disappeared during child discovery",
            ));
        }
    }
    linux_owned_process_snapshot_with(
        boundary,
        deadline,
        linux_process_info_checked,
        linux_process_children,
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone)]
enum LinuxDiscoveryAuthority {
    RetainedLeader,
    Known(ProcessIdentity),
    ChildOf(u32),
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_owned_process_snapshot_with(
    boundary: &Boundary,
    deadline: Instant,
    mut inspect: impl FnMut(u32) -> std::io::Result<Option<ProcessInfo>>,
    mut children: impl FnMut(u32, Instant) -> std::io::Result<Option<Vec<u32>>>,
) -> std::io::Result<Vec<ProcessInfo>> {
    let mut owned = BTreeMap::<u32, ProcessInfo>::new();

    loop {
        check_snapshot_deadline(deadline)?;
        let before = owned
            .values()
            .map(|process| process.identity.clone())
            .collect::<BTreeSet<_>>();
        let mut pending = VecDeque::new();
        if boundary.leader_retained {
            pending.push_back((boundary.leader, LinuxDiscoveryAuthority::RetainedLeader));
        }
        for identity in boundary
            .members
            .values()
            .chain(owned.values().map(|row| &row.identity))
        {
            pending.push_back((
                identity.pid,
                LinuxDiscoveryAuthority::Known(identity.clone()),
            ));
        }

        let mut scanned = BTreeSet::new();
        while let Some((pid, authority)) = pending.pop_front() {
            check_snapshot_deadline(deadline)?;
            let Some(process) = inspect(pid)? else {
                continue;
            };
            let known = boundary
                .members
                .get(&pid)
                .or_else(|| owned.get(&pid).map(|row| &row.identity))
                .is_some_and(|identity| *identity == process.identity);
            let belongs = match authority {
                LinuxDiscoveryAuthority::RetainedLeader => {
                    boundary.leader_retained && pid == boundary.leader
                }
                LinuxDiscoveryAuthority::Known(identity) => identity == process.identity,
                LinuxDiscoveryAuthority::ChildOf(parent) => process.ppid == parent || known,
            };
            if !belongs {
                return Err(std::io::Error::other(format!(
                    "process {pid} changed parent during local child discovery"
                )));
            }
            if !scanned.insert(process.identity.clone()) {
                continue;
            }

            owned.insert(pid, process.clone());
            if process.live {
                let descendants = children(pid, deadline)?.ok_or_else(|| {
                    std::io::Error::other(format!(
                        "owned process {pid} vanished during child discovery"
                    ))
                })?;
                pending.extend(
                    descendants
                        .into_iter()
                        .map(|child| (child, LinuxDiscoveryAuthority::ChildOf(pid))),
                );
            }
        }

        let after = owned
            .values()
            .map(|process| process.identity.clone())
            .collect::<BTreeSet<_>>();
        // A live owned process is already a conclusive non-empty result. The
        // caller will poll again, so do not require an impossible fixed point
        // while that process is intentionally forking short-lived helpers.
        if owned.values().any(|process| process.live) {
            return Ok(owned.into_values().collect());
        }
        if after == before {
            return Ok(owned.into_values().collect());
        }
    }
}

// Local child traversal descends from the retained leader and members only,
// so a leader that forks and exits between observations hides its escape:
// the kernel reparents that descendant to this supervisor, outside the
// traversed subtree. Append supervisor-adopted candidates to a local
// snapshot so the unmarked-adoptee policy can attribute them (sole active
// boundary) or fail closed (ambiguous) instead of publishing a false empty
// proof. Rows already under observation are not duplicated. A child listing
// that cannot be proven complete fails the snapshot closed.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_local_snapshot_with_supervisor_children(
    owned: Vec<ProcessInfo>,
    deadline: Instant,
    mut supervisor_children: impl FnMut(Instant) -> std::io::Result<Option<Vec<u32>>>,
    mut inspect: impl FnMut(u32) -> std::io::Result<Option<ProcessInfo>>,
) -> std::io::Result<Vec<ProcessInfo>> {
    let mut merged = owned;
    let mut seen = merged
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    let adoptees = supervisor_children(deadline)?.unwrap_or_default();
    // SAFETY: getsid observes our own session without pointers.
    let own_sid = unsafe { libc::getsid(0) };
    for pid in adoptees {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(process) = inspect(pid)? {
            // A direct child that shares our session is indistinguishable
            // from a plain supervisor spawn: only setsid moves a process out
            // of its inherited session. Merging same-session children lets
            // one test's live fixtures fail another test's leader-exit
            // observation under parallel shards. Only detached children
            // (setsid daemons and reparented escapes) are escapee-shaped and
            // may reach the unmarked-adoptee policy; an unreadable session
            // fails closed so the policy still sees the candidate.
            if own_sid >= 0 && process.sid == own_sid as u32 {
                continue;
            }
            merged.push(process);
        }
    }
    Ok(merged)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_local_or_full_snapshot(
    local: impl FnOnce() -> std::io::Result<Vec<ProcessInfo>>,
    full: impl FnOnce() -> Option<Arc<Vec<ProcessInfo>>>,
) -> Option<(Arc<Vec<ProcessInfo>>, bool)> {
    match local() {
        Ok(processes) => Some((Arc::new(processes), true)),
        Err(_) => full().map(|processes| (processes, false)),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn cached_linux_process_snapshot(deadline: Instant) -> Option<Arc<Vec<ProcessInfo>>> {
    let now = Instant::now();
    LINUX_SNAPSHOT_CACHE
        .get_or_init(SnapshotCache::default)
        .get_or_load(now, LINUX_SNAPSHOT_TTL, deadline, process_snapshot)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn fresh_linux_process_snapshot(
    requested_at: Instant,
    deadline: Instant,
) -> Option<Arc<Vec<ProcessInfo>>> {
    LINUX_SNAPSHOT_CACHE
        .get_or_init(SnapshotCache::default)
        .get_or_load(requested_at, Duration::ZERO, deadline, process_snapshot)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn registered_linux_process_snapshot(
    requested_at: Instant,
    deadline: Instant,
) -> Option<Arc<Vec<ProcessInfo>>> {
    // Exclude the spawn-to-registration window from fallback classification
    // without serializing independent launches behind a whole-procfs scan. A
    // the finite set of launches that started before enumeration completed
    // must finish registration before these rows can be classified. Launches
    // starting later are absent from this already-complete snapshot and do not
    // serialize behind it.
    let processes = fresh_linux_process_snapshot(requested_at, deadline)?;
    SPAWN_REGISTRATIONS
        .get_or_init(SpawnRegistrationRegistry::default)
        .wait_for_snapshot_registrations(deadline)?;
    Some(processes)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn fresh_process_snapshot(
    requested_at: Instant,
    deadline: Instant,
) -> Option<Arc<Vec<ProcessInfo>>> {
    let cache = PORTABLE_SNAPSHOT_CACHE.get_or_init(SnapshotCache::default);
    cache.get_or_load(requested_at, Duration::ZERO, deadline, process_snapshot)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_children(pid: u32, deadline: Instant) -> std::io::Result<Option<Vec<u32>>> {
    linux_process_children_at(std::path::Path::new("/proc"), pid, deadline)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_children_at(
    proc_root: &std::path::Path,
    pid: u32,
    deadline: Instant,
) -> std::io::Result<Option<Vec<u32>>> {
    check_snapshot_deadline(deadline)?;
    let process_dir = proc_root.join(pid.to_string());
    let task_dir = process_dir.join("task");
    match linux_task_children_interface_at(proc_root, pid)? {
        LinuxTaskChildrenInterface::Available => {}
        LinuxTaskChildrenInterface::Unavailable => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "procfs task child lists are unavailable",
            ));
        }
        LinuxTaskChildrenInterface::ProcessGone => return Ok(None),
    }
    let entries = match std::fs::read_dir(&task_dir) {
        Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
        Err(error) if procfs_process_gone(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut children = BTreeSet::new();
    for entry in entries {
        check_snapshot_deadline(deadline)?;
        let Some(task) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = task_dir.join(task.to_string()).join("children");
        let value = match std::fs::read_to_string(&path) {
            Ok(value) => value,
            // A task can disappear after the task directory was collected.
            // Discard the partial traversal so the caller falls back to one
            // complete process snapshot instead of publishing a false empty.
            Err(error) if procfs_process_gone(&error) => return Err(error),
            Err(error) => return Err(error),
        };
        for child in value.split_whitespace() {
            let child = child.parse::<u32>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid child PID in {}", path.display()),
                )
            })?;
            children.insert(child);
        }
    }
    Ok(Some(children.into_iter().collect()))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxTaskChildrenInterface {
    Available,
    Unavailable,
    ProcessGone,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_task_children_interface_at(
    proc_root: &std::path::Path,
    pid: u32,
) -> std::io::Result<LinuxTaskChildrenInterface> {
    let process_dir = proc_root.join(pid.to_string());
    let main_children = process_dir
        .join("task")
        .join(pid.to_string())
        .join("children");
    match std::fs::metadata(main_children) {
        Ok(_) => Ok(LinuxTaskChildrenInterface::Available),
        Err(error) if procfs_process_gone(&error) => match std::fs::symlink_metadata(process_dir) {
            Ok(_) => Ok(LinuxTaskChildrenInterface::Unavailable),
            Err(error) if procfs_process_gone(&error) => {
                Ok(LinuxTaskChildrenInterface::ProcessGone)
            }
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn check_snapshot_deadline(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "process discovery deadline expired",
        ))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn procfs_process_gone(error: &std::io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|errno| matches!(errno, libc::ENOENT | libc::ESRCH))
}

/// Whether an unreadable procfs entry belongs to another app.
///
/// Android denies an app's reads of other apps' stat files, and app
/// processes cannot change UID (no setuid), so a permission-denied entry
/// is provably foreign to every owned session. Linux keeps denials
/// fail-closed instead: a setuid descendant's stat stays world-readable
/// there, so an unreadable stat indicates a genuinely partial view.
#[cfg(any(target_os = "android", all(test, unix)))]
fn procfs_process_foreign(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

/// Error-path-only probe explaining why a process snapshot is unavailable.
///
/// Samples a bounded slice of the process table plus the fallback helper's
/// presence so a failed verification names its cause instead of reporting a
/// bare failure. No spawns, no sleeps, no retries: the diagnostic itself
/// must never stall teardown.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn snapshot_failure_hint() -> String {
    let mut readable = 0u32;
    let mut denied = 0u32;
    let mut missing = 0u32;
    let mut unparsable = 0u32;
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => return format!("cannot list /proc: {error}"),
    };
    for entry in entries.flatten().take(128) {
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            continue;
        };
        let Ok(pid) = text.parse::<u32>() else {
            continue;
        };
        match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => {
                if parse_linux_process_stat(pid, &stat).is_ok() {
                    readable += 1;
                } else {
                    unparsable += 1;
                }
            }
            Err(error) if procfs_process_gone(&error) => missing += 1,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                denied += 1;
            }
            Err(_) => missing += 1,
        }
    }
    #[cfg(target_os = "android")]
    let ps = "/system/bin/ps";
    #[cfg(not(target_os = "android"))]
    let ps = "/bin/ps";
    format!(
        "procfs sample: {readable} readable, {denied} denied, {missing} vanished, {unparsable} unparsable; {ps}: {}",
        if std::path::Path::new(ps).exists() {
            "present"
        } else {
            "missing"
        }
    )
}

/// Parenthesized snapshot-failure detail for cancellation errors; empty
/// where procfs sampling is unavailable.
#[cfg(unix)]
fn snapshot_failure_suffix() -> String {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let suffix = format!(" ({})", snapshot_failure_hint());
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let suffix = String::new();
    suffix
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_info_at_checked(
    pid: u32,
    process_dir: &std::path::Path,
) -> std::io::Result<Option<ProcessInfo>> {
    let stat_path = process_dir.join("stat");
    for attempt in 0..3 {
        let stat = match std::fs::read(&stat_path) {
            Ok(stat) => stat,
            Err(error) if procfs_process_gone(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        match parse_linux_process_stat(pid, &stat) {
            Ok(process) => return Ok(Some(process)),
            Err(_) if attempt < 2 => std::thread::yield_now(),
            Err(error) => return Err(error),
        }
    }
    unreachable!("the bounded procfs stat retry loop always returns")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn parse_linux_process_stat(pid: u32, stat: &[u8]) -> std::io::Result<ProcessInfo> {
    let end = stat
        .windows(2)
        .rposition(|part| part == b") ")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid procfs stat record for process {pid}"),
            )
        })?;
    let fields: Vec<_> = stat[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect();
    let parse = |index: usize| -> std::io::Result<u32> {
        fields
            .get(index)
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<u32>().ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid numeric field {index} in procfs stat record for process {pid}"
                    ),
                )
            })
    };
    let (ppid, pgid, sid) = (parse(1)?, parse(2)?, parse(3)?);
    let start = fields
        .get(19)
        .and_then(|field| std::str::from_utf8(field).ok())
        .map(str::to_owned)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing start time in procfs stat record for process {pid}"),
            )
        })?;
    let state = fields
        .first()
        .and_then(|field| field.first())
        .copied()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing state in procfs stat record for process {pid}"),
            )
        })?;
    Ok(ProcessInfo {
        pid,
        ppid,
        pgid,
        sid,
        // Both zombie (`Z`) and dead (`X`/`x`) tasks have already lost the
        // ability to mutate state. Procfs can expose the short `X` transition
        // after environment access has been revoked but before the entry
        // disappears; treating it as live would turn an unrelated completed
        // child into a false ownership ambiguity.
        live: !matches!(state, b'Z' | b'X' | b'x'),
        stopped: matches!(state, b'T' | b't'),
        identity: ProcessIdentity {
            pid,
            start: Some(start),
        },
    })
}

#[cfg(target_vendor = "apple")]
fn portable_process_info(pid: u32, identity: ProcessIdentity) -> Option<ProcessInfo> {
    let current = darwin_process_info(pid)?;
    (current.identity == identity).then_some(current)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn portable_process_info(_pid: u32, _identity: ProcessIdentity) -> Option<ProcessInfo> {
    // Unknown Unix targets without a stable identity query fail closed rather
    // than signaling a numeric PID after a userspace-only topology check.
    None
}

#[cfg(target_vendor = "apple")]
fn darwin_process_info(pid: u32) -> Option<ProcessInfo> {
    // `proc_pidinfo` includes the process generation's start time and process
    // group, while `getsid` is a separate syscall. Bracket that second lookup
    // with two matching kernel records so PID reuse or a concurrent setsid
    // cannot splice topology from different generations into one row.
    let before = darwin_bsd_info(pid)?;
    let sid = session_id(pid)?;
    let info = darwin_bsd_info(pid)?;
    if before.pbi_start_tvsec != info.pbi_start_tvsec
        || before.pbi_start_tvusec != info.pbi_start_tvusec
        || before.pbi_pgid != info.pbi_pgid
    {
        return None;
    }
    Some(ProcessInfo {
        pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        sid,
        live: info.pbi_status != libc::SZOMB,
        stopped: info.pbi_status == libc::SSTOP,
        identity: ProcessIdentity {
            pid,
            start: Some(format!(
                "{}.{:06}",
                info.pbi_start_tvsec, info.pbi_start_tvusec
            )),
        },
    })
}

#[cfg(target_vendor = "apple")]
fn darwin_bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: info owns size writable bytes and this flavor has no auxiliary
    // argument.
    let written = unsafe {
        libc::proc_pidinfo(
            pid_i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written != size || info.pbi_pid != pid {
        return None;
    }
    Some(info)
}

#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
fn parse_ps_processes(
    text: &str,
    mut expired: impl FnMut() -> bool,
    mut topology: impl FnMut(u32) -> Option<(u32, u32, Option<String>)>,
) -> Option<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for line in text.lines() {
        if expired() {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A vanished process is handled by the topology lookup below. A
        // malformed nonblank row instead invalidates the whole pass: skipping
        // it could turn a truncated child row into a false empty snapshot.
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let ppid = fields.next()?.parse::<u32>().ok()?;
        let state = fields.next()?;
        let start = fields.collect::<Vec<_>>().join(" ");
        if expired() {
            return None;
        }
        let topology = topology(pid);
        if expired() {
            return None;
        }
        let Some((pgid, sid, exact_start)) = topology else {
            continue;
        };
        processes.push(ProcessInfo {
            pid,
            ppid,
            pgid,
            sid,
            live: !state.starts_with('Z'),
            stopped: state.starts_with('T') || state.starts_with('t'),
            identity: ProcessIdentity {
                pid,
                start: exact_start.or_else(|| (!start.is_empty()).then_some(start)),
            },
        });
    }
    Some(processes)
}

#[cfg(any(test, all(unix, not(any(target_os = "linux", target_os = "android")))))]
// Captures the portable `ps` fallback without creating an unbounded helper.
fn snapshot(mut command: Command, deadline: Instant) -> Option<Vec<u8>> {
    // TEMP-DIAG-131: revert with the rest of the macOS teardown telemetry.
    #[cfg(test)]
    let _diag = DiagScope::enter("snapshot");
    use std::io::Read as _;
    use std::os::fd::OwnedFd;

    if Instant::now() >= deadline {
        return None;
    }
    let (mut reader, writer) = std::os::unix::net::UnixStream::pair().ok()?;
    reader.set_nonblocking(true).ok()?;
    let spawn_guard = spawn_registration_guard_unchecked_until(deadline).ok()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    drop(spawn_guard);
    drop(command);
    let mut bytes = Vec::new();
    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let mut chunk = [0; 8192];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then_some(bytes),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
enum PidFd {
    Open(std::os::fd::OwnedFd),
    Unsupported,
    Gone,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PidFdOpenError {
    Gone,
    Unsupported,
    Other,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn classify_pidfd_open_error(errno: i32) -> PidFdOpenError {
    match errno {
        // Some procfs/PID-namespace combinations report ENOENT rather than
        // ESRCH when the observed process vanishes before pidfd_open.
        libc::ESRCH | libc::ENOENT => PidFdOpenError::Gone,
        // Older kernels and Android seccomp profiles may not expose pidfds.
        libc::ENOSYS | libc::EINVAL | libc::EPERM => PidFdOpenError::Unsupported,
        _ => PidFdOpenError::Other,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn stable_pidfd(state: PidFd) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    match state {
        PidFd::Open(pidfd) => Ok(Some(pidfd)),
        PidFd::Gone => Ok(None),
        PidFd::Unsupported => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe descendant delivery requires pidfd support",
        )),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_pidfd(pid: u32) -> std::io::Result<PidFd> {
    use std::os::fd::FromRawFd as _;

    let Ok(pid) = i32::try_from(pid) else {
        return Ok(PidFd::Gone);
    };
    // SAFETY: pidfd_open takes one positive PID and flags=0. A successful
    // descriptor pins that exact process identity across the final validation
    // and signal delivery, closing the remaining PID-reuse window.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd >= 0 {
        // SAFETY: the successful syscall returned a newly owned descriptor.
        return Ok(PidFd::Open(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error().map(classify_pidfd_open_error) {
        Some(PidFdOpenError::Gone) => Ok(PidFd::Gone),
        // Callers fail closed rather than reopening a PID-reuse window with a
        // raw process-directed signal after userspace revalidation.
        Some(PidFdOpenError::Unsupported) => Ok(PidFd::Unsupported),
        Some(PidFdOpenError::Other) | None => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn signal_pidfd(pidfd: &std::os::fd::OwnedFd, signal: i32) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: the pidfd remains owned for the complete syscall, siginfo is
    // null for a normal process-directed signal, and flags must be zero.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == 0
    {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn probe_pidfd_wait(pidfd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // The current process is deliberately not its own child. A supported
    // P_PIDFD selector therefore reports ECHILD; older kernels report EINVAL,
    // and syscall filters can report EPERM. Only the former proves that the
    // selector required by exact adopted-child reaping is available.
    // SAFETY: the pidfd stays live, the local siginfo buffer is writable, and
    // WNOWAIT prevents any accidental consumption if kernel semantics change.
    let status = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PIDFD,
            pidfd.as_raw_fd() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if status == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ECHILD) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
// Signals one process group whose retained leader reserves its numeric ID.
fn signal_group(leader: u32, signal: i32) -> std::io::Result<bool> {
    if let Ok(leader) = i32::try_from(leader) {
        if leader > 0 {
            // SAFETY: the child was made leader of this owned process group.
            if unsafe { libc::kill(-leader, signal) } == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(false)
            } else {
                Err(error)
            };
        }
    }
    Ok(false)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
// Reads a live process's session identity.
fn session_id(pid: u32) -> Option<u32> {
    // SAFETY: getsid observes a positive process identity without pointers.
    u32::try_from(unsafe { libc::getsid(pid as i32) }).ok()
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android")),
    not(target_vendor = "apple")
))]
// Reads a live process's process-group identity.
fn process_group(pid: u32) -> Option<u32> {
    // SAFETY: getpgid observes a positive process identity without pointers.
    u32::try_from(unsafe { libc::getpgid(pid as i32) }).ok()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_has_boundary_marker_checked(pid: u32, token: &str) -> std::io::Result<Option<bool>> {
    process_boundary_marker_disposition_checked(pid, token).map(|disposition| {
        disposition.map(|disposition| disposition == BoundaryMarkerDisposition::Matches)
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_boundary_marker_disposition_checked(
    pid: u32,
    token: &str,
) -> std::io::Result<Option<BoundaryMarkerDisposition>> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(environment) => Ok(Some(boundary_marker_disposition(&environment, token))),
        Err(error) if procfs_process_gone(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_vendor = "apple")]
fn process_has_boundary_marker(pid: u32, token: &str) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0_usize;
    // SAFETY: the MIB and size pointer are valid; a null output buffer asks the
    // kernel only for the required KERN_PROCARGS2 byte count.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
        || size > 8 * 1024 * 1024
    {
        return false;
    }
    let mut environment = vec![0_u8; size];
    // SAFETY: the output buffer owns `size` writable bytes and the same valid
    // MIB is used for the second, data-producing query.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            environment.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return false;
    }
    environment.truncate(size);
    darwin_process_environment(&environment)
        .is_some_and(|environment| boundary_marker_matches(environment, token))
}

// KERN_PROCARGS2 returns argc, the executable path, alignment NULs, argv, then
// the NUL-separated environment. Parse past argv before matching the private
// boundary marker so a crafted executable path or argument cannot claim
// ownership of an unrelated process that happened to be reparented to pid 1.
#[cfg(any(test, target_vendor = "apple"))]
fn darwin_process_environment(process_args: &[u8]) -> Option<&[u8]> {
    const INT_BYTES: usize = std::mem::size_of::<libc::c_int>();
    if INT_BYTES != 4 || process_args.len() < INT_BYTES {
        return None;
    }
    let argc = i32::from_ne_bytes(process_args[..INT_BYTES].try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let mut cursor = INT_BYTES;

    cursor += process_args[cursor..].iter().position(|byte| *byte == 0)? + 1;
    while process_args.get(cursor) == Some(&0) {
        cursor += 1;
    }
    for _ in 0..argc {
        cursor += process_args[cursor..].iter().position(|byte| *byte == 0)? + 1;
    }
    Some(&process_args[cursor..])
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn process_has_boundary_marker(_pid: u32, _token: &str) -> bool {
    false
}

#[cfg(any(test, target_vendor = "apple"))]
fn boundary_marker_matches(environment: &[u8], token: &str) -> bool {
    boundary_marker_disposition(environment, token) == BoundaryMarkerDisposition::Matches
}

#[cfg(unix)]
fn boundary_marker_disposition(environment: &[u8], token: &str) -> BoundaryMarkerDisposition {
    let prefix = format!("{BOUNDARY_ENV}=");
    let mut found_marker = false;
    for entry in environment.split(|byte| *byte == 0) {
        let Some(value) = entry.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        found_marker = true;
        if value
            .split(|byte| *byte == b':')
            .any(|value| value == token.as_bytes())
        {
            return BoundaryMarkerDisposition::Matches;
        }
    }
    if found_marker {
        BoundaryMarkerDisposition::Different
    } else {
        BoundaryMarkerDisposition::Unknown
    }
}

// Makes orphaned descendants directly reapable where the kernel supports it.
fn adopt_descendants() -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: PR_SET_CHILD_SUBREAPER accepts an integer flag and no pointer.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn remember_members_for_reap(boundary: &Boundary) {
    let mut pending = PENDING_REAPS
        .get_or_init(|| Mutex::new(PendingReaps::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (&pid, identity) in &boundary.members {
        if pid != boundary.leader {
            pending.members.insert(pid, identity.clone());
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn remember_members_for_reap(_boundary: &Boundary) {}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_pending_members() -> std::io::Result<()> {
    reap_pending_members_with(
        PENDING_REAPS.get_or_init(|| Mutex::new(PendingReaps::default())),
        Instant::now(),
        |identity| reap_member(identity.pid, identity),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_unobserved_adopted_zombies(deadline: Instant) -> std::io::Result<()> {
    if !descendant_adoption_enabled()? {
        return Ok(());
    }
    ADOPTED_REAP_COORDINATOR
        .get_or_init(AdoptedReapCoordinator::default)
        .run_with(deadline, reap_unobserved_adopted_zombies_once)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone)]
struct SharedReapError {
    kind: std::io::ErrorKind,
    message: String,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SharedReapError {
    fn capture(error: std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn into_error(self) -> std::io::Error {
        std::io::Error::new(self.kind, self.message)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default)]
struct AdoptedReapState {
    completed: u64,
    running: bool,
    requested: u64,
    waiters: BTreeMap<u64, usize>,
    deadlines: BTreeMap<u64, Instant>,
    results: BTreeMap<u64, Result<(), SharedReapError>>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl AdoptedReapState {
    fn remove_waiter(&mut self, generation: u64) {
        if let Some(waiters) = self.waiters.get_mut(&generation) {
            *waiters -= 1;
            if *waiters == 0 {
                self.waiters.remove(&generation);
                self.deadlines.remove(&generation);
                self.results.remove(&generation);
            }
        }
        self.requested = self
            .waiters
            .keys()
            .next_back()
            .copied()
            .unwrap_or(self.completed);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default)]
struct AdoptedReapCoordinator {
    state: Mutex<AdoptedReapState>,
    changed: Condvar,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl AdoptedReapCoordinator {
    // Requests a sweep that begins after this call. Requests arriving during
    // one sweep share its immediately following generation, so high fan-out
    // child completion cannot serialize one global scan per worker.
    fn run_with(
        &self,
        deadline: Instant,
        mut sweep: impl FnMut(Instant) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let target = {
            let mut state = lock_until(&self.state, deadline).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaper coordination deadline expired",
                )
            })?;
            let target = state.completed + if state.running { 2 } else { 1 };
            state.requested = state.requested.max(target);
            *state.waiters.entry(target).or_default() += 1;
            state
                .deadlines
                .entry(target)
                .and_modify(|current| *current = (*current).max(deadline))
                .or_insert(deadline);
            target
        };

        loop {
            let mut state = lock_until(&self.state, deadline).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaper coordination deadline expired",
                )
            })?;
            if state.completed >= target {
                let result = state.results.get(&target).cloned().unwrap_or_else(|| {
                    Err(SharedReapError {
                        kind: std::io::ErrorKind::Other,
                        message: "adopted subprocess reaper lost a completed result".to_owned(),
                    })
                });
                state.remove_waiter(target);
                return result.map_err(SharedReapError::into_error);
            }

            if !state.running && state.completed < state.requested {
                let generation = state.completed + 1;
                let sweep_deadline = state
                    .deadlines
                    .get(&generation)
                    .copied()
                    .unwrap_or(deadline);
                state.running = true;
                drop(state);

                let result = sweep(sweep_deadline).map_err(SharedReapError::capture);
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.completed = generation;
                state.results.insert(generation, result);
                state.running = false;
                self.changed.notify_all();
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.remove_waiter(target);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaper coordination deadline expired",
                ));
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            if timeout.timed_out() && state.completed < target {
                state.remove_waiter(target);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "adopted subprocess reaper coordination deadline expired",
                ));
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
struct AdoptedReaperIntent;

#[cfg(any(target_os = "linux", target_os = "android"))]
impl AdoptedReaperIntent {
    fn acquire() -> Self {
        ADOPTED_REAPER_WAITING.store(true, Ordering::Release);
        Self
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Drop for AdoptedReaperIntent {
    fn drop(&mut self) {
        ADOPTED_REAPER_WAITING.store(false, Ordering::Release);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_unobserved_adopted_zombies_once(deadline: Instant) -> std::io::Result<()> {
    let _intent = AdoptedReaperIntent::acquire();
    // The exclusive registration guard proves that every direct child in the
    // snapshot has either been registered as an active leader or predates the
    // sweep. New owned spawns wait only for this bounded classification pass.
    let _spawn_guard = write_lock_until(&OWNED_CHILD_SPAWN, deadline).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "owned subprocess registry deadline expired",
        )
    })?;
    let active_leaders = lock_until(
        ACTIVE_BOUNDARY_LEADERS.get_or_init(|| Mutex::new(BTreeMap::new())),
        deadline,
    )
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "active subprocess registry deadline expired",
        )
    })?
    .clone();
    let mut deferred = Vec::new();
    let mut first_error = None;
    // Most commands leave no adopted child. A non-consuming waitid query
    // proves that case without enumerating procfs. If the first waitable PID
    // is another registered leader, enumerate every direct child so an
    // unregistered adopted zombie cannot remain hidden behind it.
    let first = peek_waitable_child()?;
    let children = match first {
        None => Vec::new(),
        Some(_) => match linux_supervisor_children(deadline)? {
            Some(children) => children,
            None => linux_process_snapshot_checked(deadline)?
                .into_iter()
                .filter(|process| process.ppid == std::process::id())
                .map(|process| process.pid)
                .collect(),
        },
    };
    for pid in children {
        check_snapshot_deadline(deadline)?;
        if active_leaders.contains_key(&pid) {
            continue;
        }
        let Some(process) = linux_process_info_checked(pid)? else {
            continue;
        };
        if process.ppid != std::process::id() || process.live {
            continue;
        }
        let identity = process.identity;
        match reap_member(pid, &identity) {
            MemberWaitState::Reaped | MemberWaitState::GoneOrReused | MemberWaitState::NotChild => {
            }
            MemberWaitState::Deferred | MemberWaitState::Interrupted | MemberWaitState::Running => {
                deferred.push(identity);
            }
            MemberWaitState::Failed(error) if first_error.is_none() => {
                first_error = Some(error);
            }
            MemberWaitState::Failed(_) => {}
        }
    }
    if !deferred.is_empty() {
        let mut pending = PENDING_REAPS
            .get_or_init(|| Mutex::new(PendingReaps::default()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for identity in deferred {
            pending.members.insert(identity.pid, identity);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peek_waitable_child() -> std::io::Result<Option<u32>> {
    loop {
        // SAFETY: waitid initializes the local siginfo and WNOWAIT preserves
        // the child's status for its registered boundary or exact reaper.
        let result = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let status = libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            (status, info.si_pid())
        };
        if result.0 == 0 {
            return Ok(u32::try_from(result.1).ok().filter(|pid| *pid > 0));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ECHILD) => return Ok(None),
            _ => return Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn supervisor_has_children() -> std::io::Result<bool> {
    loop {
        // SAFETY: waitid initializes only the local siginfo. WNOWAIT makes
        // this a non-consuming existence query: Linux returns ECHILD only
        // when this subreaper has no children at all, while a live child with
        // no waitable event returns success with si_pid zero.
        let result = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ECHILD) => return Ok(false),
            _ => return Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn descendant_adoption_enabled() -> std::io::Result<bool> {
    let mut enabled = 0_i32;
    // SAFETY: PR_GET_CHILD_SUBREAPER writes one integer to the supplied
    // process-owned address.
    if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(enabled != 0)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_supervisor_children(deadline: Instant) -> std::io::Result<Option<Vec<u32>>> {
    let main_children = format!("/proc/{0}/task/{0}/children", std::process::id());
    if let Err(error) = std::fs::metadata(&main_children) {
        if error.kind() == std::io::ErrorKind::NotFound {
            // CONFIG_PROC_CHILDREN is optional. Fall back to one strict fresh
            // process-table scan when the kernel does not expose it.
            return Ok(None);
        }
        return Err(error);
    }
    loop {
        match linux_supervisor_children_once(deadline) {
            Ok(children) => return Ok(Some(children)),
            Err(error) if procfs_process_gone(&error) && Instant::now() < deadline => {
                // A test/worker thread can retire between collecting the task
                // directory and reading its child list. Discard the entire
                // partial observation and retry; never publish the prefix.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_supervisor_children_once(deadline: Instant) -> std::io::Result<Vec<u32>> {
    check_snapshot_deadline(deadline)?;
    let task_dir = format!("/proc/{}/task", std::process::id());
    let entries = std::fs::read_dir(&task_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    let mut children = BTreeSet::new();
    for entry in entries {
        check_snapshot_deadline(deadline)?;
        let task = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid task entry in {task_dir}"),
                )
            })?;
        let path = format!("/proc/{}/task/{task}/children", std::process::id());
        let value = std::fs::read_to_string(&path)?;
        for child in value.split_whitespace() {
            let child = child.parse::<u32>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid child PID in {path}"),
                )
            })?;
            children.insert(child);
        }
    }
    Ok(children.into_iter().collect())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default)]
struct PendingReaps {
    members: BTreeMap<u32, ProcessIdentity>,
    next_sweep: Option<Instant>,
    cursor: Option<u32>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl PendingReaps {
    fn take_due(&mut self, now: Instant) -> Vec<ProcessIdentity> {
        if self.next_sweep.is_some_and(|next| now < next) {
            return Vec::new();
        }
        self.next_sweep = Some(now + TRACK_POLL);
        let identities = match self.cursor {
            Some(cursor) => self
                .members
                .range((
                    std::ops::Bound::Excluded(cursor),
                    std::ops::Bound::Unbounded,
                ))
                .take(PENDING_REAP_BATCH)
                .map(|(_, identity)| identity.clone())
                .collect::<Vec<_>>(),
            None => self
                .members
                .values()
                .take(PENDING_REAP_BATCH)
                .cloned()
                .collect::<Vec<_>>(),
        };
        self.cursor = if identities.len() == PENDING_REAP_BATCH {
            identities.last().map(|identity| identity.pid)
        } else {
            None
        };
        identities
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_pending_members_with(
    pending: &Mutex<PendingReaps>,
    now: Instant,
    mut reap: impl FnMut(&ProcessIdentity) -> MemberWaitState,
) -> std::io::Result<()> {
    let identities = pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take_due(now);
    if identities.is_empty() {
        return Ok(());
    }

    // pidfd_open, procfs inspection, and waitid can all be delayed by a loaded
    // system. Never hold the process-global registry mutex across them: other
    // boundaries must remain able to register adoptees and enter cancellation.
    let results = identities
        .into_iter()
        .map(|identity| {
            let result = reap(&identity);
            (identity, result)
        })
        .collect::<Vec<_>>();
    let mut pending = pending.lock().unwrap_or_else(|error| error.into_inner());
    let mut first_error = None;
    for (identity, result) in results {
        match result {
            MemberWaitState::Reaped | MemberWaitState::GoneOrReused => {
                if pending.members.get(&identity.pid) == Some(&identity) {
                    pending.members.remove(&identity.pid);
                }
            }
            MemberWaitState::Running
            | MemberWaitState::Deferred
            | MemberWaitState::Interrupted
            | MemberWaitState::NotChild => {}
            MemberWaitState::Failed(error) if first_error.is_none() => {
                first_error = Some(error);
            }
            MemberWaitState::Failed(_) => {}
        }
    }
    if pending.members.is_empty() {
        pending.cursor = None;
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
// Reaps adopted descendants in dependency order after their leader is reaped.
enum MemberWaitState {
    Reaped,
    Running,
    Deferred,
    Interrupted,
    NotChild,
    GoneOrReused,
    Failed(std::io::Error),
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_member(pid: u32, expected: &ProcessIdentity) -> MemberWaitState {
    reap_member_with(
        pid,
        expected,
        linux_process_info_checked,
        open_pidfd,
        wait_member_pidfd,
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_member_with(
    pid: u32,
    expected: &ProcessIdentity,
    inspect: impl Fn(u32) -> std::io::Result<Option<ProcessInfo>>,
    open: impl FnOnce(u32) -> std::io::Result<PidFd>,
    wait: impl FnOnce(&std::os::fd::OwnedFd) -> MemberWaitState,
) -> MemberWaitState {
    let current = match inspect(pid) {
        Ok(Some(current)) => current,
        Ok(None) => return MemberWaitState::GoneOrReused,
        Err(error) => return MemberWaitState::Failed(error),
    };
    if current.identity != *expected {
        return MemberWaitState::GoneOrReused;
    }
    if current.live {
        return MemberWaitState::Running;
    }

    let pidfd = match open(pid).and_then(stable_pidfd) {
        Ok(Some(pidfd)) => pidfd,
        Ok(None) => return MemberWaitState::GoneOrReused,
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            // A dead process retains its PID, but without an atomic wait
            // handle another waiter could reap it before a numeric wait and
            // allow that PID to be reused. Keep the identity pending rather
            // than risking another worker's child or poisoning normal command
            // completion with a permanent cleanup failure.
            return MemberWaitState::Deferred;
        }
        Err(error) => return MemberWaitState::Failed(error),
    };
    match inspect(pid) {
        Ok(Some(current)) if current.identity == *expected && !current.live => {}
        Ok(_) => return MemberWaitState::GoneOrReused,
        Err(error) => return MemberWaitState::Failed(error),
    }
    match wait(&pidfd) {
        MemberWaitState::Failed(error)
            if error.raw_os_error().is_some_and(|errno| {
                matches!(errno, libc::ENOSYS | libc::EINVAL | libc::EPERM)
            }) =>
        {
            MemberWaitState::Deferred
        }
        state => state,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait_member_pidfd(pidfd: &std::os::fd::OwnedFd) -> MemberWaitState {
    use std::os::fd::AsRawFd as _;

    // SAFETY: the pidfd pins the exact process generation validated above;
    // P_PIDFD prevents a historical numeric PID from reaping a different
    // concurrently owned child after reuse.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PIDFD,
            pidfd.as_raw_fd() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        ) == 0
        {
            return if info.si_pid() == 0 {
                MemberWaitState::Running
            } else {
                MemberWaitState::Reaped
            };
        }
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINTR) => MemberWaitState::Interrupted,
        Some(libc::ECHILD) => MemberWaitState::NotChild,
        _ => MemberWaitState::Failed(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn reap_members(boundary: &Boundary) -> std::io::Result<()> {
    let deadline = Instant::now() + GRACE;
    let mut first_error = None;
    let mut pending: Vec<_> = boundary
        .members
        .keys()
        .copied()
        .filter(|pid| *pid != boundary.leader)
        .collect();
    while !pending.is_empty() && Instant::now() < deadline {
        let mut next = Vec::new();
        let mut not_children = Vec::new();
        let mut progress = false;
        let mut child_pending = false;
        let mut interrupted = false;
        for pid in pending.drain(..) {
            match reap_member(
                pid,
                boundary.members.get(&pid).expect("pending member identity"),
            ) {
                MemberWaitState::Reaped | MemberWaitState::GoneOrReused => progress = true,
                MemberWaitState::Running => {
                    child_pending = true;
                    next.push(pid);
                }
                MemberWaitState::Deferred => next.push(pid),
                MemberWaitState::Interrupted => {
                    interrupted = true;
                    next.push(pid);
                }
                // A grandchild may become ours only after its intermediate
                // parent is reaped later in this pass.
                MemberWaitState::NotChild => not_children.push(pid),
                MemberWaitState::Failed(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if progress || child_pending || interrupted {
            next.extend(not_children);
        }
        pending = next;
        if !pending.is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if !pending.is_empty() {
        Err(std::io::Error::other(
            "owned subprocesses could not be reaped before the cleanup deadline",
        ))
    } else if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
// Other Unix kernels hand orphan reaping to init.
fn reap_members(_boundary: &Boundary) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(any(target_os = "linux", target_os = "android"))]
    static TEST_TERM_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    extern "C" fn count_test_term(_signal: i32) {
        TEST_TERM_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn install_test_term_counter() {
        TEST_TERM_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the handler has C ABI, touches only a lock-free atomic, and
        // the subprocess exits before this test returns.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = count_test_term as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            assert_eq!(
                libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()),
                0
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_restore_rejects_a_process_that_left_the_foreground_group() {
        let cached = super::ProcessInfo {
            pid: 42,
            ppid: 1,
            pgid: 42,
            sid: 1,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 42,
                start: Some("generation".to_owned()),
            },
        };
        let fresh = super::ProcessInfo {
            pgid: 43,
            ..cached.clone()
        };

        assert!(!super::fresh_process_owns_foreground_group(
            &cached,
            Some(&fresh),
            42,
        ));
        assert!(super::fresh_process_owns_foreground_group(
            &cached,
            Some(&cached),
            42,
        ));
        assert!(!super::fresh_process_owns_foreground_group(
            &cached, None, 42,
        ));
    }

    #[cfg(unix)]
    fn thread_cpu_time() -> Duration {
        let mut elapsed = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime initializes the complete local timespec.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut elapsed) },
            0
        );
        Duration::new(elapsed.tv_sec as u64, elapsed.tv_nsec as u32)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn wait_for_zombie(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if super::linux_process_info_checked(pid)
                .unwrap()
                .is_some_and(|process| !process.live)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for process {pid} to become a zombie"
            );
            std::thread::sleep(super::POLL);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn kill_and_reap_test_process(pid: u32, pidfd: &std::os::fd::OwnedFd) {
        let _ = super::signal_pidfd(pidfd, libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: the isolated test enabled subreaper adoption and this
            // exact pidfd pins the fixture identity. waitpid consumes only
            // that fixture if it has already been adopted.
            let waited = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
            if waited == pid as i32 {
                return;
            }
            if waited == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD)
                    && super::linux_process_info_checked(pid)
                        .unwrap()
                        .is_none_or(|process| !process.live)
                {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "could not reap fixture {pid}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Failure-safe cleanup for an escaped fixture whose PID may be published
    /// only after the assertion path starts unwinding.
    ///
    /// The watchdog waits on stdin for its owner to release it (drop closes
    /// the pipe), then delivers SIGKILL to the exact published (PID, start,
    /// command-line) identity. stdin EOF with a missing or unparsable pid file
    /// waits briefly for late publication; a valid file whose target is
    /// already gone exits immediately because there is nothing left to clean.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const PUBLISHED_PID_WATCHDOG_SCRIPT: &str = r#"
import os, signal, sys, time
path = sys.argv[1]
token = path.encode()
sys.stdin.buffer.read()
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    try:
        pid_text, expected_start = open(path).read().split()
        pid = int(pid_text)
    except (FileNotFoundError, ValueError):
        time.sleep(0.005)
        continue
    try:
        pidfd = os.pidfd_open(pid)
    except ProcessLookupError:
        sys.exit(0)
    except (PermissionError, OSError):
        sys.exit(0)
    try:
        stat = open(f'/proc/{pid}/stat').read().rsplit(')', 1)[1].split()
        cmdline = open(f'/proc/{pid}/cmdline', 'rb').read()
    except (FileNotFoundError, ProcessLookupError):
        os.close(pidfd)
        sys.exit(0)
    except (PermissionError, OSError):
        os.close(pidfd)
        sys.exit(0)
    if stat[19] != expected_start or token not in cmdline:
        os.close(pidfd)
        sys.exit(0)
    try:
        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    os.close(pidfd)
    sys.exit(0)
sys.exit(0)
"#;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    struct PublishedPidCleanup {
        input: Option<std::process::ChildStdin>,
        watchdog: std::process::Child,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl PublishedPidCleanup {
        fn new(path: &std::path::Path) -> Self {
            use std::os::unix::process::CommandExt as _;

            let mut command = Command::new("python3");
            command
                .args([
                    "-c",
                    PUBLISHED_PID_WATCHDOG_SCRIPT,
                    &path.display().to_string(),
                ])
                // The watchdog outlives its owner by design (stdin EOF starts
                // its grace period), including when the owner runs directly in
                // the test harness. Carry the shared non-matching harness
                // marker so concurrent boundary proofs classify it as another
                // worker's process instead of an unattributable adoptee.
                .env(super::BOUNDARY_ENV, "test-harness-subprocess")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: setsid is async-signal-safe and gives the watchdog a
            // lifetime independent of the deliberately killable test session.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            // A marker is not visible until exec. Hold the registration window
            // across fork so a concurrent proof cannot observe the markerless
            // pre-exec child and fail closed on another test's watchdog.
            let registration = super::SpawnRegistrationWindow::begin();
            let mut watchdog = command.spawn().expect("fixture watchdog should start");
            drop(registration);
            let input = watchdog.stdin.take();
            Self { input, watchdog }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl Drop for PublishedPidCleanup {
        fn drop(&mut self) {
            self.input.take();
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                match self.watchdog.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            }
            let _ = self.watchdog.kill();
            let _ = self.watchdog.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read_published_identity(path: &std::path::Path) -> Option<(u32, String)> {
        let value = std::fs::read_to_string(path).ok()?;
        let mut fields = value.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let start = fields.next()?.to_owned();
        (fields.next().is_none()).then_some((pid, start))
    }

    /// Live members of the `root` process tree (inclusive) from one `/proc`
    /// snapshot, so the forced-kill test discovers stub-fork launcher chains
    /// exactly.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn intermediate_tree(root: u32) -> std::collections::HashSet<u32> {
        fn ppid_of(pid: u32) -> Option<u32> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            stat.rsplit(')')
                .next()?
                .split_whitespace()
                .nth(1)?
                .parse::<u32>()
                .ok()
        }
        let mut children: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|text| text.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == root {
                    continue;
                }
                let Some(ppid) = ppid_of(pid) else { continue };
                children.entry(ppid).or_default().push(pid);
            }
        }
        let mut tree = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !tree.insert(pid) {
                continue;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        tree
    }

    #[cfg(unix)]
    #[test]
    fn signal_between_parent_check_and_exec_denies_child_launch() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        const CHILD_ENV: &str = "SHDEPS_TEST_PRE_EXEC_SIGNAL_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::signal_between_parent_check_and_exec_denies_child_launch";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let signals = super::Signals::install_with_restore(true).unwrap();
        let dir = crate::test_support::temp_dir("shdeps-pre-exec-cancel");
        let side_effect = dir.join("exec-ran");
        let parent = std::process::id() as i32;
        let (mut acknowledger, child_gate) = std::os::unix::net::UnixStream::pair().unwrap();
        let child_gate_fd = child_gate.as_raw_fd();
        let acknowledge = std::thread::spawn(move || {
            let mut ready = [0_u8; 1];
            acknowledger.read_exact(&mut ready).unwrap();
            while super::received_signal().is_none() {
                std::thread::yield_now();
            }
            acknowledger.write_all(&[1]).unwrap();
        });

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf executed >\"$1\"", "sh"])
            .arg(&side_effect)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // This callback runs before the production isolation callback. It
        // waits for the parent signal handler to acknowledge TERM, removing
        // scheduler timing from the pre-exec race.
        unsafe {
            command.pre_exec(move || {
                let ready = [1_u8; 1];
                if libc::kill(parent, libc::SIGTERM) != 0
                    || libc::write(child_gate_fd, ready.as_ptr().cast(), ready.len()) != 1
                {
                    return Err(std::io::Error::last_os_error());
                }
                let mut acknowledged = [0_u8; 1];
                loop {
                    let read = libc::read(
                        child_gate_fd,
                        acknowledged.as_mut_ptr().cast(),
                        acknowledged.len(),
                    );
                    if read == 1 {
                        break;
                    }
                    if read < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut spawned = super::spawn_owned(&mut command, super::Isolation::DetachedSession, true);
        drop(child_gate);
        acknowledge.join().unwrap();
        if let Ok(child) = &mut spawned {
            let _ = child.wait();
        }

        assert!(
            spawned.is_err(),
            "a signal acknowledged before the child authorization point must abort exec"
        );
        assert!(
            !side_effect.exists(),
            "the denied executable must have no side effects"
        );
        assert_eq!(signals.finish_result::<std::io::Error>(Ok(0)).unwrap(), 143);
    }

    #[cfg(unix)]
    #[test]
    fn child_directed_signal_after_authorization_cannot_reach_exec() {
        use std::os::unix::process::CommandExt as _;

        const CHILD_ENV: &str = "SHDEPS_TEST_CHILD_SIGNAL_AFTER_AUTHORIZATION";
        const TEST_NAME: &str =
            "cancellation::tests::child_directed_signal_after_authorization_cannot_reach_exec";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let _signals = super::Signals::install_with_restore(true).unwrap();
        let dir = crate::test_support::temp_dir("shdeps-child-signal-before-exec");
        for (name, isolation) in [
            ("parent-session", super::Isolation::ParentSession),
            ("detached-session", super::Isolation::DetachedSession),
        ] {
            let side_effect = dir.join(name);
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf executed >\"$1\"", "sh"])
                .arg(&side_effect)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let marker = super::isolate(&mut command, isolation);
            // This callback is intentionally registered after the production
            // authorization callback. It makes the child deliver TERM to
            // itself in the former check-to-exec gap without relying on
            // scheduler timing.
            unsafe {
                command.pre_exec(|| {
                    if libc::kill(libc::getpid(), libc::SIGTERM) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let spawned = command.spawn();
            if let Ok(child) = spawned {
                let mut child = super::OwnedChild::new(child, isolation, marker);
                let _ = child.wait();
            }

            assert!(
                !side_effect.exists(),
                "a child-directed TERM after authorization reached exec for {name}"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn supervisor_owns_sigchld_while_targets_retain_the_inherited_disposition() {
        const SUPERVISOR_ENV: &str = "SHDEPS_TEST_SIGCHLD_SUPERVISOR";
        const TARGET_ENV: &str = "SHDEPS_TEST_SIGCHLD_TARGET";
        const RESULT_ENV: &str = "SHDEPS_TEST_SIGCHLD_RESULT";
        const TEST_NAME: &str = "cancellation::tests::supervisor_owns_sigchld_while_targets_retain_the_inherited_disposition";

        if let Ok(expected) = std::env::var(TARGET_ENV) {
            // SAFETY: a null new action queries the process's current
            // disposition into initialized local storage.
            let action = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action),
                    0
                );
                action
            };
            let observed = if action.sa_sigaction == libc::SIG_IGN {
                "ignore"
            } else if action.sa_flags & libc::SA_NOCLDWAIT != 0 {
                "no-cldwait"
            } else {
                "default"
            };
            std::fs::write(std::env::var_os(RESULT_ENV).unwrap(), observed).unwrap();
            assert_eq!(observed, expected);
            std::process::exit(42);
        }

        if let Ok(expected) = std::env::var(SUPERVISOR_ENV) {
            // Model the disposition inherited at process entry. SA_NOCLDWAIT
            // itself is reset by exec on this platform, so it must be applied
            // inside the isolated supervisor fixture.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = if expected == "ignore" {
                    libc::SIG_IGN
                } else {
                    libc::SIG_DFL
                };
                if expected == "no-cldwait" {
                    action.sa_flags |= libc::SA_NOCLDWAIT;
                }
                assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                    0
                );
            }
            let signals = super::Signals::install_with_restore(true).unwrap();
            let inherited = super::target_sigchld_disposition()
                .expect("the process-entry guard must retain the target disposition");
            match expected.as_str() {
                "ignore" => assert_eq!(inherited.sa_sigaction, libc::SIG_IGN),
                "no-cldwait" => {
                    assert_ne!(inherited.sa_flags & libc::SA_NOCLDWAIT, 0);
                }
                "default" => assert_eq!(inherited.sa_sigaction, libc::SIG_DFL),
                _ => unreachable!(),
            }
            let result = crate::test_support::temp_dir("sigchld-target").join("result");
            let target_expected = if expected == "no-cldwait" {
                "default"
            } else {
                &expected
            };
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                // exec preserves SIG_IGN but resets SA_NOCLDWAIT flags on a
                // default disposition, matching direct child semantics.
                .env(TARGET_ENV, target_expected)
                .env(RESULT_ENV, &result)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let output = super::output(command, None).unwrap();
            assert_eq!(output.status.code(), Some(42));
            assert_eq!(std::fs::read_to_string(result).unwrap(), target_expected);
            drop(signals);
            let restored = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                assert_eq!(
                    libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action),
                    0
                );
                action
            };
            match expected.as_str() {
                "ignore" => assert_eq!(restored.sa_sigaction, libc::SIG_IGN),
                "no-cldwait" => assert_ne!(restored.sa_flags & libc::SA_NOCLDWAIT, 0),
                "default" => assert_eq!(restored.sa_sigaction, libc::SIG_DFL),
                _ => unreachable!(),
            }
            return;
        }

        for mode in ["default", "ignore", "no-cldwait"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(SUPERVISOR_ENV, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let output = crate::test_support::run_subprocess(command).unwrap();
            assert!(
                output.status.success(),
                "SIGCHLD fixture {mode} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_signal_after_result_selection_is_not_ignored() {
        const CHILD_ENV: &str = "SHDEPS_TEST_TERMINAL_SIGNAL_HANDOFF";
        const TEST_NAME: &str =
            "cancellation::tests::terminal_signal_after_result_selection_is_not_ignored";
        if let Ok(mode) = std::env::var(CHILD_ENV) {
            let signals = super::Signals::install().unwrap();
            if mode == "cleanup" {
                super::record_cleanup_error(&std::io::Error::other("injected cleanup failure"));
            } else if mode == "first-signal" {
                // SAFETY: this child owns the installed process signal handler.
                assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
            }
            let injected = match mode.as_str() {
                "hup" => Some(libc::SIGHUP),
                "int" => Some(libc::SIGINT),
                "quit" => Some(libc::SIGQUIT),
                "term" | "cleanup" | "first-signal" => Some(libc::SIGTERM),
                "fallback" => None,
                _ => panic!("unknown terminal-handoff fixture mode: {mode}"),
            };
            signals.exit_process_with(7, |installed| {
                if injected == Some(installed) {
                    // SAFETY: handled signals are blocked until every terminal
                    // disposition is installed, making this injection exact.
                    assert_eq!(unsafe { libc::raise(installed) }, 0);
                }
            });
        }

        for (mode, expected) in [
            ("hup", 128 + libc::SIGHUP),
            ("int", 128 + libc::SIGINT),
            ("quit", 128 + libc::SIGQUIT),
            ("term", 128 + libc::SIGTERM),
            ("cleanup", 1),
            ("first-signal", 128 + libc::SIGHUP),
            ("fallback", 7),
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD_ENV, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let status = crate::test_support::run_subprocess(command).unwrap().status;
            assert_eq!(status.code(), Some(expected), "fixture mode {mode}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn closed_capture_channels_wait_without_busy_spinning() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exec 1>&- 2>&-; sleep 0.4");

        let wall_started = Instant::now();
        let cpu_started = thread_cpu_time();
        let output = super::output(command, None).unwrap();
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_started);
        let wall_elapsed = wall_started.elapsed();

        assert!(output.status.success());
        assert!(wall_elapsed >= Duration::from_millis(300));
        // The completion slow path performs whole-system snapshots whose cost
        // scales with /proc size, so assert the actual invariant (no busy
        // spin: cpu well under wall) instead of a fixed absolute budget. A
        // busy spin would show cpu ~= wall; the wall floor above keeps the
        // threshold at >= 150ms.
        assert!(
            cpu_elapsed < wall_elapsed / 2,
            "a disconnected activity channel must retain bounded polling; cpu={cpu_elapsed:?}, wall={wall_elapsed:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn concurrent_owned_spawns_do_not_serialize_registration() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;
        use std::sync::{Arc, Barrier};

        const SPAWNS: usize = 8;
        let (mut ready_reader, ready_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (release_reader, mut release_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        ready_reader.set_nonblocking(true).unwrap();
        let start = Arc::new(Barrier::new(SPAWNS + 1));
        let mut workers = Vec::new();

        for _ in 0..SPAWNS {
            let start = Arc::clone(&start);
            let ready_fd = ready_writer.as_raw_fd();
            let release_fd = release_reader.as_raw_fd();
            workers.push(std::thread::spawn(move || {
                let mut command = Command::new("/bin/true");
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                // Keep every forked child between spawn and exec until the
                // parent observes all launch attempts. The owned-spawn
                // registry must permit these registrations concurrently.
                unsafe {
                    command.pre_exec(move || {
                        let byte = [1_u8];
                        loop {
                            let written = libc::write(ready_fd, byte.as_ptr().cast(), byte.len());
                            if written == 1 {
                                break;
                            }
                            if written < 0
                                && std::io::Error::last_os_error().kind()
                                    == std::io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            return Err(std::io::Error::last_os_error());
                        }
                        let mut release = [0_u8; 1];
                        loop {
                            let read =
                                libc::read(release_fd, release.as_mut_ptr().cast(), release.len());
                            if read == 1 {
                                return Ok(());
                            }
                            if read < 0
                                && std::io::Error::last_os_error().kind()
                                    == std::io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            return Err(std::io::Error::last_os_error());
                        }
                    });
                }
                start.wait();
                let mut child = super::spawn_owned_with_foreground(
                    &mut command,
                    super::Isolation::ExactChild,
                    true,
                    false,
                )
                .unwrap();
                child.wait().unwrap()
            }));
        }

        start.wait();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut ready = 0_usize;
        let mut buffer = [0_u8; SPAWNS];
        while ready < SPAWNS && Instant::now() < deadline {
            match ready_reader.read(&mut buffer[ready..]) {
                Ok(0) => break,
                Ok(count) => ready += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("could not read spawn readiness: {error}"),
            }
        }
        release_writer.write_all(&[1_u8; SPAWNS]).unwrap();
        for worker in workers {
            assert!(worker.join().unwrap().success());
        }

        assert_eq!(
            ready, SPAWNS,
            "owned child registration serialized independent spawn calls"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn fallback_snapshot_waits_for_spawn_registration_or_its_deadline() {
        let (ready_sender, ready) = std::sync::mpsc::channel();
        let (release_sender, release) = std::sync::mpsc::channel();
        let spawner = std::thread::spawn(move || {
            let registration = super::SpawnRegistrationWindow::begin();
            let mut command = Command::new("/bin/sleep");
            command
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut unregistered = command.spawn().unwrap();
            ready_sender.send(unregistered.id()).unwrap();
            release.recv().unwrap();
            drop(registration);
            unregistered.kill().unwrap();
            unregistered.wait().unwrap()
        });
        let unregistered_pid = ready.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        let snapshot =
            super::registered_linux_process_snapshot(started, started + Duration::from_millis(50));
        let elapsed = started.elapsed();
        release_sender.send(()).unwrap();
        let _ = spawner.join().unwrap();

        assert!(
            snapshot.is_none(),
            "fallback discovery observed child {unregistered_pid} before registration"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "spawn registration barrier ignored the caller deadline: {elapsed:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn raw_test_fixture_helper_registers_before_exec() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        let (mut ready_reader, ready_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (release_reader, mut release_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let ready_fd = ready_writer.as_raw_fd();
        let release_fd = release_reader.as_raw_fd();
        let fixture = std::thread::spawn(move || {
            let _ready_writer = ready_writer;
            let _release_reader = release_reader;
            let mut command = Command::new("/bin/true");
            // Hold the raw fixture in its inherited pre-exec environment. A
            // marker-only helper would still be invisible at this point.
            unsafe {
                command.pre_exec(move || {
                    let ready = [1_u8];
                    if libc::write(ready_fd, ready.as_ptr().cast(), ready.len()) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut release = [0_u8; 1];
                    if libc::read(release_fd, release.as_mut_ptr().cast(), release.len()) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            crate::test_support::run_subprocess(command)
        });
        let mut ready = [0_u8; 1];
        ready_reader.read_exact(&mut ready).unwrap();

        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let observer = std::thread::spawn(move || {
            observed_tx
                .send(super::registered_linux_process_snapshot(
                    Instant::now(),
                    Instant::now() + Duration::from_secs(1),
                ))
                .unwrap();
        });
        assert!(
            observed_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a pre-exec fixture must remain behind the registration frontier"
        );
        release_writer.write_all(&[1]).unwrap();
        assert!(
            observed_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_some(),
            "snapshot must complete after the fixture registers"
        );
        observer.join().unwrap();
        assert!(fixture.join().unwrap().unwrap().status.success());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn concurrent_adopted_reap_requests_share_a_completed_generation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        const REQUESTS: usize = 32;
        let coordinator = Arc::new(super::AdoptedReapCoordinator::default());
        let start = Arc::new(Barrier::new(REQUESTS + 1));
        let sweeps = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..REQUESTS {
            let coordinator = Arc::clone(&coordinator);
            let observer = Arc::clone(&coordinator);
            let start = Arc::clone(&start);
            let sweeps = Arc::clone(&sweeps);
            workers.push(std::thread::spawn(move || {
                start.wait();
                coordinator.run_with(Instant::now() + Duration::from_secs(2), |_| {
                    let sweep = sweeps.fetch_add(1, Ordering::SeqCst);
                    if sweep == 0 {
                        let deadline = Instant::now() + Duration::from_secs(1);
                        loop {
                            let registered = observer
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .waiters
                                .values()
                                .sum::<usize>();
                            if registered == REQUESTS {
                                break;
                            }
                            assert!(
                                Instant::now() < deadline,
                                "concurrent reap requests did not register"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    Ok(())
                })
            }));
        }

        start.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(
            sweeps.load(Ordering::SeqCst),
            2,
            "requests that arrive during one sweep must share one fresh follow-up sweep"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn normal_wait_reaps_an_unobserved_adopted_zombie_without_stealing_another_leader() {
        const CHILD_ENV: &str = "SHDEPS_TEST_NORMAL_IMMEDIATE_ZOMBIE_CHILD";
        const TEST_NAME: &str = "cancellation::tests::normal_wait_reaps_an_unobserved_adopted_zombie_without_stealing_another_leader";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-immediate-adopted-zombie");
        let orphan_pid_path = dir.join("orphan.pid");

        let mut other_command = Command::new("/bin/true");
        other_command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let other_marker = super::isolate(&mut other_command, super::Isolation::ExactChild);
        let mut other = super::OwnedChild::new(
            other_command.spawn().unwrap(),
            super::Isolation::ExactChild,
            other_marker,
        );
        while !other.exited().unwrap() {
            std::thread::yield_now();
        }

        let script = format!(
            "import os, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n open({orphan_pid_path:?}, 'w').write(str(os.getpid()))\n os._exit(0)\nwhile not os.path.exists({orphan_pid_path:?}):\n time.sleep(0.001)\nos._exit(0)\n"
        );
        let mut command = Command::new("python3");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let orphan_pid = loop {
            if let Ok(value) = std::fs::read_to_string(&orphan_pid_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        wait_for_zombie(orphan_pid);
        while !child.exited().unwrap() {
            std::thread::yield_now();
        }
        assert!(child.wait().unwrap().success());

        let orphan_reaped = super::linux_process_info_checked(orphan_pid)
            .unwrap()
            .is_none();
        if !orphan_reaped {
            // SAFETY: this test process is the configured subreaper and the
            // fixture has confirmed this exact PID is an adopted zombie.
            unsafe {
                libc::waitpid(orphan_pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
        let other_status = other.wait();
        assert!(
            orphan_reaped,
            "an immediately exited adopted descendant with empty procfs environment was not reaped"
        );
        assert!(
            other_status.is_ok_and(|status| status.success()),
            "global adopted-child cleanup must not reap another active boundary leader"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn cancellation_reaps_an_unobserved_adopted_zombie_with_empty_environment() {
        const CHILD_ENV: &str = "SHDEPS_TEST_CANCEL_IMMEDIATE_ZOMBIE_CHILD";
        const TEST_NAME: &str = "cancellation::tests::cancellation_reaps_an_unobserved_adopted_zombie_with_empty_environment";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        super::adopt_descendants().unwrap();
        let signals = super::Signals::install_with_restore(true).unwrap();
        let dir = crate::test_support::temp_dir("shdeps-cancel-immediate-zombie");
        let orphan_pid_path = dir.join("orphan.pid");
        let script = format!(
            "import os, signal, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n open({orphan_pid_path:?}, 'w').write(str(os.getpid()))\n os._exit(0)\nsignal.signal(signal.SIGTERM, lambda *_: os._exit(0))\nwhile True:\n time.sleep(1)\n"
        );
        let mut command = Command::new("python3");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let orphan_pid = loop {
            if let Ok(value) = std::fs::read_to_string(&orphan_pid_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        wait_for_zombie(orphan_pid);
        // A zombie's environment is empty on procfs, so marker matching alone
        // cannot attribute this adopted descendant. Kernels that release the
        // zombie's memory before its procfs directory goes away fail the read
        // with ESRCH instead; either way no marker is readable, matching
        // production's procfs_process_gone classification.
        match std::fs::read(format!("/proc/{orphan_pid}/environ")) {
            Ok(environment) => assert!(environment.is_empty()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) if super::procfs_process_gone(&error) => {}
            Err(error) => panic!(
                "zombie environ must be empty or unreadable, got {error:?} (errno {:?})",
                error.raw_os_error()
            ),
        }

        // SAFETY: this isolated process installed the production TERM handler.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        child.stop(libc::SIGTERM).unwrap();
        let orphan_reaped = super::linux_process_info_checked(orphan_pid)
            .unwrap()
            .is_none();
        if !orphan_reaped {
            // SAFETY: this test process is the subreaper for the confirmed
            // adopted zombie and must clean it before reporting failure.
            unsafe {
                libc::waitpid(orphan_pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }

        assert!(orphan_reaped, "cancellation left an adopted zombie behind");
        assert_eq!(signals.finish_result::<std::io::Error>(Ok(0)).unwrap(), 143);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn live_close_lease_escape_remains_visible_to_boundary_reconciliation() {
        const CHILD_ENV: &str = "SHDEPS_TEST_LIVE_CLOSE_LEASE_ESCAPE_CHILD";
        const TEST_NAME: &str = "cancellation::tests::live_close_lease_escape_remains_visible_to_boundary_reconciliation";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        struct PidfdGuard(std::os::fd::OwnedFd);
        impl Drop for PidfdGuard {
            fn drop(&mut self) {
                let _ = super::signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-live-close-lease-escape");
        let descendant_path = dir.join("descendant.pid");
        let _fixture_cleanup = PublishedPidCleanup::new(&descendant_path);
        let script = format!(
            "import os, signal, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n os.closerange(3, 1024)\n start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]\n open({descendant_path:?}, 'w').write(f'{{os.getpid()}} {{start}}')\n while True: time.sleep(1)\nwhile not os.path.exists({descendant_path:?}): time.sleep(0.001)\nos._exit(0)\n"
        );
        let mut command = Command::new("python3");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let descendant = loop {
            if let Some((pid, _)) = read_published_identity(&descendant_path) {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        let super::PidFd::Open(descendant_pidfd) = super::open_pidfd(descendant).unwrap() else {
            panic!("fixture descendant must have stable pidfd authority");
        };
        let _descendant_guard = PidfdGuard(descendant_pidfd);
        let leader_deadline = Instant::now() + Duration::from_secs(2);
        while super::observe_exit(child.child.as_mut().unwrap())
            .unwrap()
            .is_none()
        {
            assert!(Instant::now() < leader_deadline);
            std::thread::sleep(super::POLL);
        }

        let lease_closed = child.boundary.lifetime_has_holders().unwrap() == Some(false);
        let marker_visible = super::process_has_boundary_marker_checked(
            descendant,
            child.boundary.marker.token.as_deref().unwrap(),
        );
        let reconciled = child
            .boundary
            .reconcile_fresh(Instant::now() + Duration::from_secs(1));
        let retained = child.boundary.members.contains_key(&descendant);
        let cleanup = child.stop(libc::SIGKILL);

        assert!(
            lease_closed,
            "the fixture must close its inherited lifetime lease"
        );
        assert_eq!(
            reconciled,
            Some(false),
            "live marker visibility before reconciliation: {marker_visible:?}"
        );
        assert!(
            retained,
            "a live escaped descendant with the boundary marker must remain attributable"
        );
        cleanup.unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn missing_task_children_falls_back_for_unmarked_closed_lease_escape() {
        const CHILD_ENV: &str = "SHDEPS_TEST_MISSING_CHILDREN_FALLBACK_CHILD";
        const TEST_NAME: &str = "cancellation::tests::missing_task_children_falls_back_for_unmarked_closed_lease_escape";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        struct PidfdGuard(std::os::fd::OwnedFd);
        impl Drop for PidfdGuard {
            fn drop(&mut self) {
                let _ = super::signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-missing-children-escape");
        let descendant_path = dir.join("descendant.pid");
        let script = format!(
            "import os, signal, time\nos.closerange(3, 1024)\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n open({descendant_path:?}, 'w').write(str(os.getpid()))\n while True: time.sleep(1)\n[signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\nwhile not os.path.exists({descendant_path:?}): time.sleep(0.001)\nwhile True: time.sleep(1)\n"
        );
        let mut command = Command::new("/usr/bin/env");
        command
            .args(["-i", "PATH=/usr/bin:/bin", "python3", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let descendant = loop {
            if let Ok(value) = std::fs::read_to_string(&descendant_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        let super::PidFd::Open(descendant_pidfd) = super::open_pidfd(descendant).unwrap() else {
            panic!("fixture descendant must have stable pidfd authority");
        };
        let _descendant_guard = PidfdGuard(descendant_pidfd);
        assert_eq!(
            child.boundary.lifetime_has_holders().unwrap(),
            Some(false),
            "both fixture processes must close the inherited lifetime lease"
        );
        assert_eq!(
            super::process_has_boundary_marker_checked(
                descendant,
                child.boundary.marker.token.as_deref().unwrap()
            )
            .unwrap(),
            Some(false),
            "the escaped fixture must exec without the boundary marker"
        );

        let cleanup = child.stop(libc::SIGTERM);
        let descendant_survived = super::linux_process_info_checked(descendant)
            .unwrap()
            .is_some_and(|process| process.live);

        assert!(cleanup.is_ok(), "cleanup failed: {cleanup:?}");
        assert!(
            !descendant_survived,
            "fallback discovery missed an unmarked, closed-lease escaped descendant"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn drained_completion_attributes_sole_unmarked_closed_lease_adoptee_after_reap() {
        const CHILD_ENV: &str = "SHDEPS_TEST_UNMARKED_ADOPTEE_CHILD";
        const TEST_NAME: &str = "cancellation::tests::drained_completion_attributes_sole_unmarked_closed_lease_adoptee_after_reap";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        struct PidfdGuard(std::os::fd::OwnedFd);
        impl Drop for PidfdGuard {
            fn drop(&mut self) {
                let _ = super::signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-unmarked-adoptee");
        let descendant_path = dir.join("descendant.pid");
        let script = format!(
            "import os, signal, time\nos.closerange(3, 1024)\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n open({descendant_path:?}, 'w').write(str(os.getpid()))\n while True: time.sleep(1)\nwhile not os.path.exists({descendant_path:?}): time.sleep(0.001)\nos._exit(0)\n"
        );
        let mut command = Command::new("/usr/bin/env");
        command
            .args(["-i", "PATH=/usr/bin:/bin", "python3", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let leader = child.child.as_ref().unwrap().id();
        let started = Instant::now();
        let descendant = loop {
            if let Ok(value) = std::fs::read_to_string(&descendant_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        let super::PidFd::Open(descendant_pidfd) = super::open_pidfd(descendant).unwrap() else {
            panic!("fixture descendant must have stable pidfd authority");
        };
        let _descendant_guard = PidfdGuard(descendant_pidfd);
        let leader_deadline = Instant::now() + Duration::from_secs(2);
        while super::linux_process_info_checked(leader)
            .unwrap()
            .is_some_and(|process| process.live)
        {
            assert!(Instant::now() < leader_deadline);
            std::thread::sleep(super::POLL);
        }
        assert_eq!(child.boundary.lifetime_has_holders().unwrap(), Some(false));
        assert_eq!(
            super::process_has_boundary_marker_checked(
                descendant,
                child.boundary.marker.token.as_deref().unwrap()
            )
            .unwrap(),
            Some(false)
        );

        let status = loop {
            if let Some(status) = child.wait_if_exited_and_output_drained().unwrap() {
                break status;
            }
            std::thread::sleep(super::POLL);
        };
        assert!(status.success());
        assert!(
            child.boundary.members.contains_key(&descendant),
            "the sole direct adoptee must remain attributable after its leader is reaped"
        );
        super::stop_reaped_boundary(&mut child.boundary, libc::SIGTERM, None).unwrap();
        child.cleanup_complete = true;
        assert!(
            super::linux_process_info_checked(descendant)
                .unwrap()
                .is_none_or(|process| !process.live),
            "the attributed adoptee survived cleanup"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn sole_boundary_does_not_claim_adoptee_with_a_different_marker() {
        const CHILD_ENV: &str = "SHDEPS_TEST_DIFFERENT_MARKER_ADOPTEE_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::sole_boundary_does_not_claim_adoptee_with_a_different_marker";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        struct PidfdGuard(std::os::fd::OwnedFd);
        impl Drop for PidfdGuard {
            fn drop(&mut self) {
                let _ = super::signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-different-marker-adoptee");
        let descendant_path = dir.join("descendant.pid");
        let script = format!(
            "import os, signal, time\nos.closerange(3, 1024)\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n open({descendant_path:?}, 'w').write(str(os.getpid()))\n while True: time.sleep(1)\nwhile not os.path.exists({descendant_path:?}): time.sleep(0.001)\nos._exit(0)\n"
        );
        let mut command = Command::new("/usr/bin/env");
        command
            .args([
                "-i",
                "PATH=/usr/bin:/bin",
                "SHDEPS_INTERNAL_PROCESS_BOUNDARIES=another-boundary-token",
                "python3",
                "-c",
                &script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let leader = child.child.as_ref().unwrap().id();
        let started = Instant::now();
        let descendant = loop {
            if let Ok(value) = std::fs::read_to_string(&descendant_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        let super::PidFd::Open(descendant_pidfd) = super::open_pidfd(descendant).unwrap() else {
            panic!("fixture descendant must have stable pidfd authority");
        };
        let descendant_guard = PidfdGuard(descendant_pidfd);
        let leader_deadline = Instant::now() + Duration::from_secs(2);
        while super::linux_process_info_checked(leader)
            .unwrap()
            .is_some_and(|process| process.live)
        {
            assert!(Instant::now() < leader_deadline);
            std::thread::sleep(super::POLL);
        }

        assert!(child.exited().unwrap());
        assert!(
            !child.boundary.members.contains_key(&descendant),
            "a known different boundary marker must never fall through to unmarked adoption"
        );
        assert!(child.wait().unwrap().success());
        assert!(
            super::linux_process_info_checked(descendant)
                .unwrap()
                .is_some_and(|process| process.live),
            "the boundary stole and signaled another marker's live process"
        );
        kill_and_reap_test_process(descendant, &descendant_guard.0);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn unmarked_adoptee_is_not_claimed_across_active_boundaries() {
        const CHILD_ENV: &str = "SHDEPS_TEST_AMBIGUOUS_UNMARKED_ADOPTEE_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::unmarked_adoptee_is_not_claimed_across_active_boundaries";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let first_leader = u32::MAX - 2;
        let second_leader = u32::MAX - 1;
        let first = super::Boundary::new(
            first_leader,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("first-boundary".to_owned())),
        );
        let second = super::Boundary::new(
            second_leader,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("second-boundary".to_owned())),
        );
        let rows = [
            super::ProcessInfo {
                pid: first_leader,
                ppid: std::process::id(),
                pgid: first_leader,
                sid: first_leader,
                live: false,
                stopped: false,
                identity: super::ProcessIdentity {
                    pid: first_leader,
                    start: Some("first-leader".to_owned()),
                },
            },
            super::ProcessInfo {
                pid: second_leader,
                ppid: std::process::id(),
                pgid: second_leader,
                sid: second_leader,
                live: false,
                stopped: false,
                identity: super::ProcessIdentity {
                    pid: second_leader,
                    start: Some("second-leader".to_owned()),
                },
            },
            super::ProcessInfo {
                pid: u32::MAX,
                ppid: std::process::id(),
                pgid: u32::MAX,
                sid: u32::MAX,
                live: true,
                stopped: false,
                identity: super::ProcessIdentity {
                    pid: u32::MAX,
                    start: Some("ambiguous-adoptee".to_owned()),
                },
            },
        ];
        let by_pid = rows
            .iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();

        for boundary in [&first, &second] {
            assert!(
                !boundary
                    .contains_unmarked_adoptee_with(
                        &rows[2],
                        &by_pid,
                        Instant::now() + Duration::from_secs(1),
                        false,
                        |_| Ok(Some(rows[2].clone())),
                        |_| Ok(super::BoundaryMarkerDisposition::Unknown),
                    )
                    .unwrap(),
                "an unmarked adoptee must not be assigned while another boundary could own it"
            );
            assert!(
                boundary
                    .contains_unmarked_adoptee_with(
                        &rows[2],
                        &by_pid,
                        Instant::now() + Duration::from_secs(1),
                        true,
                        |_| Ok(Some(rows[2].clone())),
                        |_| Ok(super::BoundaryMarkerDisposition::Unknown),
                    )
                    .is_err(),
                "strict cleanup must not acknowledge an unattributable live adoptee"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn adoptee_that_exits_during_settle_is_not_ambiguous() {
        // A short-lived unrelated child can overlap two back-to-back
        // inspections yet exit before the failure branch runs. The policy
        // must re-verify after a settle instead of failing closed on the
        // stale row: there is nothing left to attribute or clean up.
        let _first = super::Boundary::new(
            u32::MAX - 10,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("first-boundary".to_owned())),
        );
        let second = super::Boundary::new(
            u32::MAX - 11,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("second-boundary".to_owned())),
        );
        let dead_leader = |pid: u32, start: &str| super::ProcessInfo {
            pid,
            ppid: std::process::id(),
            pgid: pid,
            sid: pid,
            live: false,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(start.to_owned()),
            },
        };
        let transient = super::ProcessInfo {
            pid: u32::MAX - 12,
            ppid: std::process::id(),
            pgid: u32::MAX - 12,
            sid: u32::MAX - 12,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: u32::MAX - 12,
                start: Some("transient-generation".to_owned()),
            },
        };
        let first_dead = dead_leader(u32::MAX - 10, "first-leader");
        let second_dead = dead_leader(u32::MAX - 11, "second-leader");
        let by_pid = std::collections::BTreeMap::from([
            (first_dead.pid, &first_dead),
            (second_dead.pid, &second_dead),
            (transient.pid, &transient),
        ]);
        let inspections = Cell::new(0_u8);
        assert!(
            !second
                .contains_unmarked_adoptee_with(
                    &transient,
                    &by_pid,
                    Instant::now() + Duration::from_secs(1),
                    true,
                    |_| {
                        inspections.set(inspections.get() + 1);
                        if inspections.get() <= 2 {
                            Ok(Some(transient.clone()))
                        } else {
                            Ok(None)
                        }
                    },
                    |_| Ok(super::BoundaryMarkerDisposition::Unknown),
                )
                .unwrap(),
            "an adoptee that exits during the ambiguity settle must not fail the proof"
        );
        assert_eq!(inspections.get(), 3);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn ambiguous_same_session_adoptee_is_not_failed_closed() {
        // A same-session direct child is indistinguishable from a plain
        // supervisor spawn: only setsid moves a process out of its
        // inherited session. Failing closed on one lets a concurrent
        // test's live fixture fail another test's leader-exit
        // observation. Detached adoptees (the escapee shape) still fail
        // closed; same-session rows are ignored when ambiguous. Sole
        // attribution upstream is unaffected.
        // SAFETY: getsid observes our own session without pointers.
        let own_sid = unsafe { libc::getsid(0) } as u32;
        let _first = super::Boundary::new(
            u32::MAX - 20,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("first-boundary".to_owned())),
        );
        let second = super::Boundary::new(
            u32::MAX - 21,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("second-boundary".to_owned())),
        );
        let dead_leader = |pid: u32, start: &str| super::ProcessInfo {
            pid,
            ppid: std::process::id(),
            pgid: pid,
            sid: pid,
            live: false,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(start.to_owned()),
            },
        };
        let plain = super::ProcessInfo {
            pid: u32::MAX - 22,
            ppid: std::process::id(),
            pgid: u32::MAX - 22,
            sid: own_sid,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: u32::MAX - 22,
                start: Some("plain-generation".to_owned()),
            },
        };
        let first_dead = dead_leader(u32::MAX - 20, "first-leader");
        let second_dead = dead_leader(u32::MAX - 21, "second-leader");
        let by_pid = std::collections::BTreeMap::from([
            (first_dead.pid, &first_dead),
            (second_dead.pid, &second_dead),
            (plain.pid, &plain),
        ]);
        assert!(
            !second
                .contains_unmarked_adoptee_with(
                    &plain,
                    &by_pid,
                    Instant::now() + Duration::from_secs(1),
                    true,
                    |_| Ok(Some(plain.clone())),
                    |_| Ok(super::BoundaryMarkerDisposition::Unknown),
                )
                .unwrap(),
            "an ambiguous same-session adoptee must not fail the proof closed"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn reused_leader_pid_registration_survives_old_boundary_release() {
        const CHILD_ENV: &str = "SHDEPS_TEST_REUSED_LEADER_REGISTRY_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::reused_leader_pid_registration_survives_old_boundary_release";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let pid = u32::MAX - 7;
        let mut old = super::Boundary::new(
            pid,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("old-generation".to_owned())),
        );
        let mut reused = super::Boundary::new(
            pid,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("reused-generation".to_owned())),
        );

        old.release_leader();
        assert!(
            super::ACTIVE_BOUNDARY_LEADERS
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&pid),
            "releasing an old PID generation must retain the replacement registration"
        );
        reused.release_leader();
        assert!(
            !super::ACTIVE_BOUNDARY_LEADERS
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&pid),
            "the final registration release must remove the PID"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn ambiguous_live_adoptee_makes_cleanup_incomplete_without_stealing_a_leader() {
        const CHILD_ENV: &str = "SHDEPS_TEST_AMBIGUOUS_LIVE_ADOPTEE_CHILD";
        const TEST_NAME: &str = "cancellation::tests::ambiguous_live_adoptee_makes_cleanup_incomplete_without_stealing_a_leader";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        struct PidfdGuard(std::os::fd::OwnedFd);
        impl Drop for PidfdGuard {
            fn drop(&mut self) {
                let _ = super::signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        super::adopt_descendants().unwrap();
        let mut unrelated_command = Command::new("/bin/sleep");
        unrelated_command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let unrelated_marker =
            super::isolate(&mut unrelated_command, super::Isolation::DetachedSession);
        let mut unrelated = super::OwnedChild::new(
            unrelated_command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            unrelated_marker,
        );
        let unrelated_pid = unrelated.child.as_ref().unwrap().id();

        let dir = crate::test_support::temp_dir("shdeps-ambiguous-live-adoptee");
        let descendant_path = dir.join("descendant.pid");
        let script = format!(
            "import os, signal, time\nos.closerange(3, 1024)\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n open({descendant_path:?}, 'w').write(str(os.getpid()))\n while True: time.sleep(1)\nwhile not os.path.exists({descendant_path:?}): time.sleep(0.001)\nos._exit(0)\n"
        );
        let mut command = Command::new("/usr/bin/env");
        command
            .args(["-i", "PATH=/usr/bin:/bin", "python3", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let leader = child.child.as_ref().unwrap().id();
        let started = Instant::now();
        let descendant = loop {
            if let Ok(value) = std::fs::read_to_string(&descendant_path) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(super::POLL);
        };
        let super::PidFd::Open(descendant_pidfd) = super::open_pidfd(descendant).unwrap() else {
            panic!("fixture descendant must have stable pidfd authority");
        };
        let descendant_guard = PidfdGuard(descendant_pidfd);
        let leader_deadline = Instant::now() + Duration::from_secs(2);
        while super::linux_process_info_checked(leader)
            .unwrap()
            .is_some_and(|process| process.live)
        {
            assert!(Instant::now() < leader_deadline);
            std::thread::sleep(super::POLL);
        }

        let normal_completion = child.exited().and_then(|exited| {
            if exited {
                child.wait().map(Some)
            } else {
                Ok(None)
            }
        });
        assert!(
            normal_completion.as_ref().is_err_and(|error| error
                .to_string()
                .contains("could not attribute adopted subprocess")),
            "normal completion must fail closed before wait, got {normal_completion:?}"
        );
        assert!(
            !child.boundary.members.contains_key(&descendant),
            "normal observation must not steal an ambiguous adoptee"
        );
        assert!(
            super::linux_process_info_checked(unrelated_pid)
                .unwrap()
                .is_some_and(|process| process.live),
            "cleanup signaled another active boundary leader"
        );
        assert!(
            super::linux_process_info_checked(descendant)
                .unwrap()
                .is_some_and(|process| process.live),
            "ambiguous ownership must not authorize signaling the adoptee"
        );

        kill_and_reap_test_process(descendant, &descendant_guard.0);
        child.stop(libc::SIGKILL).unwrap();
        unrelated.stop(libc::SIGKILL).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn reader_thread_setup_failure_stops_and_reaps_the_owned_process_tree() {
        for second_reader in [false, true] {
            let dir = crate::test_support::temp_dir(if second_reader {
                "shdeps-second-reader-spawn-failure"
            } else {
                "shdeps-first-reader-spawn-failure"
            });
            let descendant_path = dir.join("descendant.pid");
            let _fixture_cleanup = PublishedPidCleanup::new(&descendant_path);
            let script = format!(
                "import os, signal, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT)]\n start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]\n open({descendant_path:?}, 'w').write(f'{{os.getpid()}} {{start}}')\n while True: time.sleep(1)\nwhile True: time.sleep(1)\n"
            );
            let mut command = Command::new("python3");
            command
                .args(["-c", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child =
                super::spawn_owned(&mut command, super::Isolation::DetachedSession, true).unwrap();
            let leader = child.child.as_ref().unwrap().id();
            let started = Instant::now();
            let descendant = loop {
                if let Some((pid, _)) = read_published_identity(&descendant_path) {
                    break pid;
                }
                assert!(started.elapsed() < Duration::from_secs(2));
                child.observe_boundary();
                std::thread::sleep(super::POLL);
            };
            let fail_at = if second_reader { 2 } else { 1 };
            let calls = Cell::new(0_usize);
            let result = super::spawn_output_readers_with(
                &mut child,
                Box::new(|| Ok(Vec::new())),
                Box::new(|| Ok(Vec::new())),
                |name, read| {
                    calls.set(calls.get() + 1);
                    if calls.get() == fail_at {
                        Err(std::io::Error::other(format!(
                            "injected {name} reader thread creation failure"
                        )))
                    } else {
                        std::thread::Builder::new().spawn(read)
                    }
                },
            );
            assert!(result.is_err());
            let stopped = !super::linux_process_info_checked(leader)
                .unwrap()
                .is_some_and(|process| process.live)
                && !super::linux_process_info_checked(descendant)
                    .unwrap()
                    .is_some_and(|process| process.live);
            if !stopped {
                let _ = child.stop(libc::SIGKILL);
            }
            assert!(
                stopped,
                "a {} reader creation failure left the owned tree running",
                if second_reader { "second" } else { "first" }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn boundary_marker_requires_an_exact_token_in_the_inherited_chain() {
        let environment = b"PATH=/bin\0SHDEPS_INTERNAL_PROCESS_BOUNDARIES=11-1:11-20\0X=y\0";

        assert!(super::boundary_marker_matches(environment, "11-1"));
        assert!(super::boundary_marker_matches(environment, "11-20"));
        assert!(!super::boundary_marker_matches(environment, "11-2"));
        assert!(!super::boundary_marker_matches(environment, "1-1"));
        assert_eq!(
            super::boundary_marker_disposition(environment, "other-boundary"),
            super::BoundaryMarkerDisposition::Different
        );
        assert_eq!(
            super::boundary_marker_disposition(b"PATH=/bin\0", "11-1"),
            super::BoundaryMarkerDisposition::Unknown
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn vanished_snapshot_row_is_not_an_ambiguous_live_adoptee() {
        let boundary = super::Boundary::new(
            u32::MAX - 1,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("test-boundary".to_owned())),
        );
        let _other = super::Boundary::new(
            u32::MAX - 2,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("other-boundary".to_owned())),
        );
        let vanished = super::ProcessInfo {
            pid: u32::MAX,
            ppid: std::process::id(),
            pgid: u32::MAX,
            sid: u32::MAX,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: u32::MAX,
                start: Some("stale-snapshot-generation".to_owned()),
            },
        };
        let dead_leader = super::ProcessInfo {
            pid: u32::MAX - 1,
            ppid: std::process::id(),
            pgid: u32::MAX - 1,
            sid: u32::MAX - 1,
            live: false,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: u32::MAX - 1,
                start: Some("dead-leader-generation".to_owned()),
            },
        };

        assert_eq!(
            boundary.adopted_marker_disposition(&vanished).unwrap(),
            super::BoundaryMarkerDisposition::Gone,
            "a process that vanished after the snapshot must not become an unknown live adoptee"
        );
        let by_pid = std::collections::BTreeMap::from([
            (dead_leader.pid, &dead_leader),
            (vanished.pid, &vanished),
        ]);
        let inspections = Cell::new(0_u8);
        assert!(
            !boundary
                .contains_unmarked_adoptee_with(
                    &vanished,
                    &by_pid,
                    Instant::now() + Duration::from_secs(1),
                    true,
                    |_| {
                        inspections.set(inspections.get() + 1);
                        if inspections.get() == 1 {
                            Ok(Some(vanished.clone()))
                        } else {
                            Ok(None)
                        }
                    },
                    |_| Ok(super::BoundaryMarkerDisposition::Unknown),
                )
                .unwrap(),
            "a process reaped after marker inspection must not become an ambiguous live adoptee"
        );
        assert_eq!(inspections.get(), 2);
        let inspections = Cell::new(0_u8);
        let unreadable = boundary.contains_unmarked_adoptee_with(
            &vanished,
            &by_pid,
            Instant::now() + Duration::from_secs(1),
            true,
            |_| {
                inspections.set(inspections.get() + 1);
                if inspections.get() == 1 {
                    Ok(Some(vanished.clone()))
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                }
            },
            |_| Ok(super::BoundaryMarkerDisposition::Unknown),
        );
        assert!(
            unreadable.is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
            "an unreadable still-live identity must remain fail closed"
        );
    }

    #[test]
    fn boundary_tokens_do_not_repeat_after_a_process_id_is_reused() {
        assert_ne!(
            super::boundary_token("process-nonce-one", 0),
            super::boundary_token("process-nonce-two", 0),
            "a fresh process nonce must distinguish the first boundary in two process lifetimes"
        );
        assert_ne!(
            super::boundary_token("process-nonce-one", 0),
            super::boundary_token("process-nonce-one", 1),
            "the per-process counter must distinguish concurrent boundaries"
        );
    }

    #[test]
    fn darwin_boundary_marker_is_matched_only_in_environment() {
        let mut process_args = 2_i32.to_ne_bytes().to_vec();
        process_args.extend_from_slice(
            b"/tmp/SHDEPS_INTERNAL_PROCESS_BOUNDARIES=11-1\0\0sh\0SHDEPS_INTERNAL_PROCESS_BOUNDARIES=11-2\0OTHER=value\0SHDEPS_INTERNAL_PROCESS_BOUNDARIES=11-3\0",
        );
        let environment =
            super::darwin_process_environment(&process_args).expect("valid KERN_PROCARGS2 data");

        assert!(!super::boundary_marker_matches(environment, "11-1"));
        assert!(!super::boundary_marker_matches(environment, "11-2"));
        assert!(super::boundary_marker_matches(environment, "11-3"));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn exact_retained_identity_survives_a_later_topology_change() {
        let pid = std::process::id();
        let actual = super::linux_process_info_checked(pid)
            .unwrap()
            .expect("test process must be inspectable");
        let leader = pid.saturating_add(1_000_000);
        let mut stale = actual.clone();
        stale.pgid = leader;
        let mut boundary = super::Boundary::new(
            leader,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(None),
        );
        boundary.members.insert(pid, actual.identity.clone());
        boundary.current.insert(pid, stale);

        assert!(
            boundary.revalidated_process(pid).unwrap().is_some(),
            "an exact retained start-time identity remains owned after it leaves the original topology"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn descendant_escaping_after_snapshot_selects_exact_pid_authority() {
        let stale = super::ProcessInfo {
            pid: 42,
            ppid: 41,
            pgid: 41,
            sid: 40,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 42,
                start: Some("generation-one".to_owned()),
            },
        };
        let mut current = stale.clone();
        current.pgid = 42;
        current.sid = 42;

        assert_eq!(
            super::linux_signal_authority(true, 41, &current),
            super::LinuxSignalAuthority::ExactProcess,
            "delivery must classify the immediately revalidated topology, not the stale snapshot"
        );
        assert_eq!(
            super::linux_signal_authority(true, 41, &stale),
            super::LinuxSignalAuthority::RetainedGroup
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_group_delivery_per_phase_covers_group_members_without_duplicate_new_signals() {
        let leader = 41;
        let process = |pid, pgid| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid,
            sid: leader,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };
        let initial = vec![
            process(leader, leader),
            process(42, leader),
            process(43, 43),
        ];
        let group_calls = Cell::new(0_u32);
        let exact_calls = std::cell::RefCell::new(Vec::new());

        let (mut phase, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &initial,
            libc::SIGTERM,
            false,
            |group, signal| {
                assert_eq!(group, leader);
                assert_eq!(signal, libc::SIGTERM);
                group_calls.set(group_calls.get() + 1);
                Ok(true)
            },
            |process| process.pgid == leader,
            |current, signal| {
                assert_eq!(signal, libc::SIGTERM);
                exact_calls.borrow_mut().push(current.pid);
                Ok(true)
            },
        );
        result.unwrap();

        assert_eq!(group_calls.get(), 1);
        assert_eq!(*exact_calls.borrow(), vec![43]);
        assert_eq!(phase.signaled.len(), 3);

        let later = vec![process(44, leader), process(45, 45)];
        super::deliver_new_signal_phase(
            Some(leader),
            &later,
            libc::SIGTERM,
            &mut phase,
            |process| process.pgid == leader,
            |current, signal| {
                assert_eq!(signal, libc::SIGTERM);
                exact_calls.borrow_mut().push(current.pid);
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(
            group_calls.get(),
            1,
            "exact late delivery avoids a group replay"
        );
        assert_eq!(
            *exact_calls.borrow(),
            vec![43, 44, 45],
            "every newly discovered identity needs delivery in the current phase"
        );
        assert!(phase.signaled.contains(&later[0].identity));
        assert!(phase.signaled.contains(&later[1].identity));
    }

    #[cfg(unix)]
    #[test]
    fn exact_initial_and_late_cohorts_receive_each_catchable_signal_once() {
        let leader = 41;
        let process = |pid| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid: leader,
            sid: leader,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };

        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            let initial_leader = process(leader);
            // Model one child that forked after the snapshot but before
            // delivery, and another forked by the leader's signal handler.
            // Neither was in the initial cohort.
            let gap_child = process(42);
            let handler_child = process(43);
            let deliveries =
                std::cell::RefCell::new(std::collections::BTreeMap::<u32, usize>::new());
            let group_calls = Cell::new(0_usize);

            let (mut phase, result) = super::deliver_initial_signal_phase(
                leader,
                true,
                std::slice::from_ref(&initial_leader),
                signal,
                true,
                |_, delivered_signal| {
                    assert_eq!(delivered_signal, signal);
                    group_calls.set(group_calls.get() + 1);
                    Ok(true)
                },
                |process| process.pgid == leader,
                |process, delivered_signal| {
                    assert_eq!(delivered_signal, signal);
                    *deliveries.borrow_mut().entry(process.pid).or_default() += 1;
                    Ok(true)
                },
            );
            result.unwrap();
            super::deliver_new_signal_phase(
                Some(leader),
                &[gap_child.clone(), handler_child.clone()],
                signal,
                &mut phase,
                |process| process.pgid == leader,
                |process, delivered_signal| {
                    assert_eq!(delivered_signal, signal);
                    *deliveries.borrow_mut().entry(process.pid).or_default() += 1;
                    Ok(true)
                },
            )
            .unwrap();
            // A later poll must not replay delivery to any retained identity.
            super::deliver_new_signal_phase(
                Some(leader),
                &[initial_leader.clone(), gap_child, handler_child],
                signal,
                &mut phase,
                |process| process.pgid == leader,
                |process, delivered_signal| {
                    assert_eq!(delivered_signal, signal);
                    *deliveries.borrow_mut().entry(process.pid).or_default() += 1;
                    Ok(true)
                },
            )
            .unwrap();

            assert_eq!(group_calls.get(), 0, "signal {signal}");
            assert_eq!(
                *deliveries.borrow(),
                std::collections::BTreeMap::from([(leader, 1), (42, 1), (43, 1)]),
                "signal {signal}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn dead_retained_leader_does_not_force_live_cohort_onto_group_delivery() {
        let leader = 41;
        let process = |pid, live| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid: leader,
            sid: leader,
            live,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };
        let dead_leader = process(leader, false);
        let stable_child = process(42, true);
        let gap_child = process(43, true);
        let group_calls = Cell::new(0_usize);
        let deliveries = std::cell::RefCell::new(Vec::new());

        let (mut phase, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &[dead_leader, stable_child.clone()],
            libc::SIGTERM,
            true,
            |_, _| {
                group_calls.set(group_calls.get() + 1);
                Ok(true)
            },
            |process| process.pgid == leader,
            |process, _| {
                deliveries.borrow_mut().push(process.pid);
                Ok(true)
            },
        );
        result.unwrap();
        super::deliver_new_signal_phase(
            Some(leader),
            std::slice::from_ref(&gap_child),
            libc::SIGTERM,
            &mut phase,
            |process| process.pgid == leader,
            |process, _| {
                deliveries.borrow_mut().push(process.pid);
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(group_calls.get(), 0);
        assert_eq!(*deliveries.borrow(), vec![stable_child.pid, gap_child.pid]);
    }

    #[cfg(unix)]
    #[test]
    fn no_exact_authority_defers_late_members_without_replaying_catchable_group_signal() {
        let leader = 41;
        let process = |pid| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid: leader,
            sid: leader,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };
        let group_calls = Cell::new(0_usize);
        let (mut phase, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &[process(leader)],
            libc::SIGTERM,
            false,
            |_, _| {
                group_calls.set(group_calls.get() + 1);
                Ok(true)
            },
            |process| process.pgid == leader,
            |_, _| unreachable!("the initial group covers its snapshotted members"),
        );
        result.unwrap();
        let late = [process(42), process(43)];
        super::deliver_new_signal_phase(
            Some(leader),
            &late,
            libc::SIGTERM,
            &mut phase,
            |process| process.pgid == leader,
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "injected missing exact authority",
                ))
            },
        )
        .unwrap();

        assert_eq!(group_calls.get(), 1, "only the initial group TERM is sent");
        assert!(
            late.iter()
                .all(|process| !phase.signaled.contains(&process.identity)),
            "late members remain for the bounded KILL phase"
        );
        let killed_group = Cell::new(false);
        let (_, kill_result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &[process(leader), late[0].clone(), late[1].clone()],
            libc::SIGKILL,
            false,
            |_, signal| {
                assert_eq!(signal, libc::SIGKILL);
                killed_group.set(true);
                Ok(true)
            },
            |process| process.pgid == leader,
            |_, _| unreachable!("the retained group covers the final cohort"),
        );
        kill_result.unwrap();
        assert!(
            killed_group.get(),
            "the fallback KILL must cover late members"
        );
        assert_eq!(
            group_calls.get(),
            1,
            "the final KILL must not replay TERM to the leader"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn exact_grace_signals_leader_and_handler_forked_child_once() {
        const OUTER_ENV: &str = "SHDEPS_TEST_EXACT_TERM_COUNTS_OUTER";
        const TEST_NAME: &str =
            "cancellation::tests::exact_grace_signals_leader_and_handler_forked_child_once";
        const ROLE_ENV: &str = "SHDEPS_TEST_EXACT_TERM_COUNT_ROLE";
        const READY_ENV: &str = "SHDEPS_TEST_EXACT_TERM_COUNT_READY";
        const LEADER_COUNT_ENV: &str = "SHDEPS_TEST_EXACT_TERM_LEADER_COUNT";
        const CHILD_READY_ENV: &str = "SHDEPS_TEST_EXACT_TERM_CHILD_READY";
        const CHILD_COUNT_ENV: &str = "SHDEPS_TEST_EXACT_TERM_CHILD_COUNT";
        fn record_count(path: &std::path::Path, count: usize) {
            use std::io::Write as _;

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{count}").unwrap();
        }
        if std::env::var_os(OUTER_ENV).is_none() && std::env::var_os(ROLE_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, OUTER_ENV);
            return;
        }
        if let Ok(role) = std::env::var(ROLE_ENV) {
            let ready = std::path::PathBuf::from(std::env::var(READY_ENV).unwrap());
            let leader_count = std::path::PathBuf::from(std::env::var(LEADER_COUNT_ENV).unwrap());
            let child_ready = std::path::PathBuf::from(std::env::var(CHILD_READY_ENV).unwrap());
            let child_count = std::path::PathBuf::from(std::env::var(CHILD_COUNT_ENV).unwrap());
            install_test_term_counter();
            if role == "child" {
                std::fs::write(&child_ready, std::process::id().to_string()).unwrap();
                while TEST_TERM_COUNT.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let mut recorded = 0;
                let deadline = Instant::now() + Duration::from_millis(100);
                while Instant::now() < deadline {
                    let count = TEST_TERM_COUNT.load(std::sync::atomic::Ordering::SeqCst);
                    if count != recorded {
                        record_count(&child_count, count);
                        recorded = count;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                return;
            }

            std::fs::write(&ready, std::process::id().to_string()).unwrap();
            while TEST_TERM_COUNT.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut recorded = TEST_TERM_COUNT.load(std::sync::atomic::Ordering::SeqCst);
            record_count(&leader_count, recorded);
            let mut descendant = Command::new(std::env::current_exe().unwrap());
            descendant
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(ROLE_ENV, "child")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[expect(
                clippy::zombie_processes,
                reason = "the outer owned boundary reaps this intentionally inherited descendant"
            )]
            let _descendant = descendant.spawn().unwrap();
            let child_deadline = Instant::now() + Duration::from_secs(2);
            while !child_ready.is_file() {
                assert!(Instant::now() < child_deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            loop {
                let count = TEST_TERM_COUNT.load(std::sync::atomic::Ordering::SeqCst);
                if count != recorded {
                    record_count(&leader_count, count);
                    recorded = count;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        if !super::exact_descendant_authority_available() {
            // The adjacent fallback test deterministically exercises the
            // retained-group behavior used by old/restricted kernels.
            return;
        }

        let dir = crate::test_support::temp_dir("shdeps-exact-term-counts");
        let ready_path = dir.join("leader.ready");
        let leader_terms = dir.join("leader.terms");
        let child_ready = dir.join("child.ready");
        let child_terms = dir.join("child.terms");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(ROLE_ENV, "leader")
            .env(READY_ENV, &ready_path)
            .env(LEADER_COUNT_ENV, &leader_terms)
            .env(CHILD_READY_ENV, &child_ready)
            .env(CHILD_COUNT_ENV, &child_terms)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let ready_deadline = Instant::now() + Duration::from_secs(2);
        while !ready_path.is_file() {
            assert!(Instant::now() < ready_deadline);
            std::thread::sleep(super::POLL);
        }

        child.stop(libc::SIGTERM).unwrap();
        let count = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .next_back()
                .unwrap_or_default()
                .parse::<usize>()
                .unwrap_or_default()
        };
        assert_eq!(count(&leader_terms), 1, "leader received repeated TERM");
        assert_eq!(
            count(&child_terms),
            1,
            "handler-created child did not receive exactly one TERM"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn failed_initial_snapshot_still_signals_leader_and_clears_error_on_kill_proof() {
        // The injection below blocks only the registered-fallback snapshot
        // path: kernels with task-children traversal reconcile without
        // consulting the spawn registry, so the window cannot force the
        // initial snapshot to fail there and the clearing path would pass
        // vacuously. Skip loudly on such kernels rather than assert
        // nothing; the downgrade on those kernels is guarded by external
        // loaded-suite runs, not this test.
        if let Ok(super::LinuxTaskChildrenInterface::Available) =
            super::linux_task_children_interface_at(
                std::path::Path::new("/proc"),
                std::process::id(),
            )
        {
            eprintln!(
                "SKIP: task-children interface present; spawn-registration window cannot force snapshot failure"
            );
            return;
        }
        const CHILD_ENV: &str = "SHDEPS_TEST_FAILED_INITIAL_SNAPSHOT_CHILD";
        const TEST_NAME: &str = "cancellation::tests::failed_initial_snapshot_still_signals_leader_and_clears_error_on_kill_proof";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let dir = crate::test_support::temp_dir("shdeps-failed-initial-snapshot");
        let ready = dir.join("leader.ready");
        let term = dir.join("leader.term");
        let script = format!(
            "trap 'printf term > {term:?}; exit 0' TERM\nprintf ready > {ready:?}\nwhile :; do /bin/sleep 0.02; done\n"
        );
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let ready_deadline = Instant::now() + Duration::from_secs(2);
        while !ready.is_file() {
            assert!(Instant::now() < ready_deadline);
            std::thread::sleep(super::POLL);
        }

        let (blocked_sender, blocked) = std::sync::mpsc::channel();
        let term_for_blocker = term.clone();
        let blocker = std::thread::spawn(move || {
            let _registration = super::SpawnRegistrationWindow::begin();
            blocked_sender.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !term_for_blocker.is_file() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        blocked.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            !term.is_file(),
            "blocker must still hold the snapshot window when stop begins"
        );

        let cleanup = child.stop(libc::SIGTERM);
        blocker.join().unwrap();

        assert!(
            term.is_file(),
            "retained leader did not receive fallback TERM"
        );
        // The registration window forces the initial snapshot to fail, but the
        // window clears before KILL verification, which then proves the
        // boundary empty. A pre-proof retained error is definitionally
        // non-fatal once that proof succeeds, so cleanup must succeed rather
        // than report CLEANUP_FAILED for a complete cleanup.
        let status = cleanup.expect(
            "cleanup must succeed once KILL verification proves empty despite the failed initial snapshot",
        );
        assert!(
            status.success(),
            "leader trapped TERM and exited cleanly: {status:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_member_that_escapes_during_delivery_gets_an_exact_signal() {
        let leader = 41;
        let stale = super::ProcessInfo {
            pid: 42,
            ppid: leader,
            pgid: leader,
            sid: leader,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 42,
                start: Some("generation-42".to_owned()),
            },
        };
        let group_delivered = Cell::new(false);
        let exact_calls = std::cell::RefCell::new(Vec::new());

        let (phase, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            std::slice::from_ref(&stale),
            libc::SIGKILL,
            false,
            |_, _| {
                group_delivered.set(true);
                Ok(true)
            },
            |process| {
                assert!(group_delivered.get());
                assert_eq!(process.pid, stale.pid);
                false
            },
            |process, _| {
                exact_calls.borrow_mut().push(process.pid);
                Ok(true)
            },
        );

        result.unwrap();
        assert_eq!(*exact_calls.borrow(), vec![stale.pid]);
        assert!(phase.signaled.contains(&stale.identity));
    }

    #[cfg(unix)]
    #[test]
    fn one_failed_exact_delivery_does_not_skip_other_owned_identities() {
        let leader = 41;
        let escaped = |pid| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid: pid,
            sid: pid,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };
        let exact_calls = std::cell::RefCell::new(Vec::new());

        let (_, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &[escaped(42), escaped(43)],
            libc::SIGTERM,
            false,
            |_, _| Ok(true),
            |_| false,
            |process, _| {
                exact_calls.borrow_mut().push(process.pid);
                if process.pid == 42 {
                    Err(std::io::Error::other("injected delivery failure"))
                } else {
                    Ok(true)
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(
            *exact_calls.borrow(),
            vec![42, 43],
            "one failed exact delivery must not skip cleanup of another owned identity"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_exact_delivery_preserves_successful_phase_bookkeeping() {
        let leader = 41;
        let escaped = |pid| super::ProcessInfo {
            pid,
            ppid: leader,
            pgid: pid,
            sid: pid,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(format!("generation-{pid}")),
            },
        };
        let failed = escaped(42);
        let delivered = escaped(43);

        let (phase, result) = super::deliver_initial_signal_phase(
            leader,
            true,
            &[failed, delivered.clone()],
            libc::SIGTERM,
            false,
            |_, _| Ok(true),
            |_| false,
            |process, _| {
                if process.pid == 42 {
                    Err(std::io::Error::other("injected delivery failure"))
                } else {
                    Ok(true)
                }
            },
        );

        assert!(result.is_err());
        assert!(phase.group_delivered);
        assert!(
            phase.signaled.contains(&delivered.identity),
            "a later signal_new pass must not repeat an exact delivery that already succeeded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn portable_delivery_requires_the_retained_original_process_group() {
        let expected = super::ProcessInfo {
            pid: 42,
            ppid: 1,
            pgid: 41,
            sid: 40,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 42,
                start: Some("generation-one".to_owned()),
            },
        };
        let mut escaped = expected.clone();
        escaped.pgid = 42;
        assert_eq!(
            super::portable_signal_group_for(41, &expected, &escaped),
            None
        );

        let mut reused = expected.clone();
        reused.identity.start = Some("generation-two".to_owned());
        assert_eq!(
            super::portable_signal_group_for(41, &expected, &reused),
            None
        );

        assert_eq!(
            super::portable_signal_group_for(41, &expected, &expected),
            Some(41),
            "only the leader-retained original group is a pinned portable signal authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reaped_leader_no_longer_claims_reused_pid_group_or_session_topology() {
        let leader = 41;
        let original = super::ProcessInfo {
            pid: leader,
            ppid: 1,
            pgid: leader,
            sid: leader,
            live: false,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: leader,
                start: Some("original-generation".to_owned()),
            },
        };
        let reused = super::ProcessInfo {
            live: true,
            identity: super::ProcessIdentity {
                pid: leader,
                start: Some("reused-generation".to_owned()),
            },
            ..original.clone()
        };
        let reused_group_member = super::ProcessInfo {
            pid: 42,
            ppid: 1,
            pgid: leader,
            sid: leader,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 42,
                start: Some("unrelated-generation".to_owned()),
            },
        };

        for isolation in [
            super::Isolation::ExactChild,
            super::Isolation::ParentSession,
            super::Isolation::DetachedSession,
        ] {
            let mut boundary = super::Boundary::new(
                leader,
                isolation,
                super::BoundaryMarker::without_lifetime(None),
            );
            boundary
                .members
                .insert(original.pid, original.identity.clone());
            boundary.current.insert(original.pid, original.clone());
            boundary.leader_retained = false;

            boundary
                .observe_processes(&[reused.clone(), reused_group_member.clone()])
                .unwrap();

            assert!(
                boundary.current.is_empty(),
                "a reaped leader must not seed ownership or authorize matching its historical PGID/SID"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn successful_stop_clears_the_retained_leader_before_drop() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );

        let _ = child.stop(libc::SIGTERM).unwrap();

        assert!(child.child.is_none());
        assert!(
            child.cleanup_complete,
            "successful explicit teardown must suppress Drop from replaying cleanup"
        );
        assert!(
            !child.boundary.leader_retained,
            "a successfully reaped leader must release numeric PID/group/session authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_leader_wait_releases_numeric_authority() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let registration = super::SpawnRegistrationWindow::begin();
        let mut child = command.spawn().unwrap();
        let mut boundary = super::Boundary::new(
            child.id(),
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(None),
        );
        #[cfg(any(target_os = "linux", target_os = "android"))]
        drop(registration);

        assert!(
            super::wait_leader(&mut child, &mut boundary)
                .unwrap()
                .success()
        );
        assert!(!boundary.leader_retained);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn current_owned_topology_survives_a_cleared_boundary_marker() {
        let pid = std::process::id();
        let actual = super::linux_process_info_checked(pid)
            .unwrap()
            .expect("test process must be inspectable");
        let mut boundary = super::Boundary::new(
            actual.pgid,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(Some("marker-cleared-by-child".to_owned())),
        );
        boundary.members.insert(pid, actual.identity.clone());
        boundary.current.insert(pid, actual);

        assert!(
            boundary.revalidated_process(pid).unwrap().is_some(),
            "live membership in the original owned topology must remain attributable when a child clears its environment"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn unsupported_pidfd_delivery_fails_closed_before_raw_pid_signal() {
        let error = super::stable_pidfd(super::PidFd::Unsupported).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("pidfd"));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn vanished_namespace_pid_is_classified_as_gone() {
        assert_eq!(
            super::classify_pidfd_open_error(libc::ENOENT),
            super::PidFdOpenError::Gone,
            "procfs can expose a namespace PID that vanishes before pidfd_open"
        );
        assert_eq!(
            super::classify_pidfd_open_error(libc::ESRCH),
            super::PidFdOpenError::Gone
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn exact_descendant_authority_requires_every_pidfd_primitive() {
        let supported =
            super::runtime_cancellation_capability_with(|| Ok(()), |_| Ok(()), |_| Ok(()));
        assert!(supported);

        for failed_stage in 0..3 {
            let available = super::runtime_cancellation_capability_with(
                || {
                    if failed_stage == 0 {
                        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    if failed_stage == 1 {
                        Err(std::io::Error::from_raw_os_error(libc::EPERM))
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    if failed_stage == 2 {
                        Err(std::io::Error::from_raw_os_error(libc::EINVAL))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(
                !available,
                "exact PID authority must be declined when primitive {failed_stage} is unavailable"
            );
        }
        assert!(
            super::owned_subprocess_cancellation_available(),
            "Unix keeps the truthful fail-closed acknowledgement contract even when exact authority is unavailable"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn historical_member_identity_cannot_reap_a_reused_child_pid() {
        let registration = super::SpawnRegistrationWindow::begin();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .env(super::BOUNDARY_ENV, "test-harness-subprocess")
            .spawn()
            .unwrap();
        drop(registration);
        let pid = child.id();
        let mut stale = super::linux_process_info_checked(pid)
            .unwrap()
            .expect("newly spawned child identity must be inspectable")
            .identity;
        stale.start = Some("stale-process-generation".to_owned());

        assert!(matches!(
            super::reap_member(pid, &stale),
            super::MemberWaitState::GoneOrReused
        ));
        assert!(
            child.wait().is_ok(),
            "identity mismatch must leave the current child for its real owner"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn vanished_pending_reap_is_gone_before_pidfd_is_required() {
        let expected = super::ProcessIdentity {
            pid: 424_242,
            start: Some("historical-generation".to_owned()),
        };
        let opens = Cell::new(0_u32);

        let state = super::reap_member_with(
            expected.pid,
            &expected,
            |_| Ok(None),
            |_| {
                opens.set(opens.get() + 1);
                Ok(super::PidFd::Unsupported)
            },
            |_| panic!("a vanished member has no handle to wait on"),
        );

        assert!(matches!(state, super::MemberWaitState::GoneOrReused));
        assert_eq!(opens.get(), 0, "absence must be resolved before pidfd_open");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn pidfdless_pending_zombie_is_deferred_without_a_cleanup_failure() {
        let expected = super::ProcessIdentity {
            pid: 424_243,
            start: Some("retained-generation".to_owned()),
        };
        let current = super::ProcessInfo {
            pid: expected.pid,
            ppid: std::process::id(),
            pgid: expected.pid,
            sid: expected.pid,
            live: false,
            stopped: false,
            identity: expected.clone(),
        };

        let state = super::reap_member_with(
            expected.pid,
            &expected,
            |_| Ok(Some(current.clone())),
            |_| Ok(super::PidFd::Unsupported),
            |_| panic!("an unsupported pidfd cannot be waited on"),
        );

        assert!(
            matches!(state, super::MemberWaitState::Deferred),
            "normal completion must retain a safely deferred zombie without poisoning a later unrelated signal result"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn pending_zombie_defers_when_pidfd_wait_is_unavailable() {
        let expected = super::ProcessIdentity {
            pid: 424_244,
            start: Some("retained-generation".to_owned()),
        };
        let current = super::ProcessInfo {
            pid: expected.pid,
            ppid: std::process::id(),
            pgid: expected.pid,
            sid: expected.pid,
            live: false,
            stopped: false,
            identity: expected.clone(),
        };

        let state = super::reap_member_with(
            expected.pid,
            &expected,
            |_| Ok(Some(current.clone())),
            |_| {
                Ok(super::PidFd::Open(
                    std::fs::File::open("/dev/null").unwrap().into(),
                ))
            },
            |_| super::MemberWaitState::Failed(std::io::Error::from_raw_os_error(libc::EINVAL)),
        );

        assert!(
            matches!(state, super::MemberWaitState::Deferred),
            "a kernel without waitid(P_PIDFD) must defer rather than poison normal completion"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn pending_reap_sweeps_are_rate_limited_and_bounded() {
        let now = Instant::now();
        let mut pending = super::PendingReaps::default();
        for pid in 1..=(super::PENDING_REAP_BATCH as u32 + 5) {
            pending.members.insert(
                pid,
                super::ProcessIdentity {
                    pid,
                    start: Some(format!("generation-{pid}")),
                },
            );
        }

        let first = pending.take_due(now);
        assert_eq!(first.len(), super::PENDING_REAP_BATCH);
        assert!(
            pending
                .take_due(now + super::TRACK_POLL - Duration::from_millis(1))
                .is_empty(),
            "hot boundary polling must not rescan global pending children"
        );
        let second = pending.take_due(now + super::TRACK_POLL);
        assert_eq!(
            second.len(),
            5,
            "bounded sweeps must rotate without starvation"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn pending_reap_syscalls_run_outside_the_global_mutex() {
        let now = Instant::now();
        let pending = std::sync::Arc::new(std::sync::Mutex::new(super::PendingReaps::default()));
        pending.lock().unwrap().members.insert(
            42,
            super::ProcessIdentity {
                pid: 42,
                start: Some("generation-42".to_owned()),
            },
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_pending = std::sync::Arc::clone(&pending);
        let worker = std::thread::spawn(move || {
            super::reap_pending_members_with(&worker_pending, now, |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                super::MemberWaitState::Running
            })
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(
            pending.try_lock().is_ok(),
            "a slow wait syscall must not serialize unrelated boundary tracking"
        );
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn normal_completion_reaps_later_adopted_descendants_without_stealing_another_leader() {
        const CHILD_ENV: &str = "SHDEPS_TEST_NORMAL_ADOPTED_REAP_CHILD";
        const TEST_NAME: &str = "cancellation::tests::normal_completion_reaps_later_adopted_descendants_without_stealing_another_leader";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-normal-adopted-reap");
        let orphan_pid_path = dir.join("orphan.pid");
        let script = format!(
            "child = __import__('os').fork()\nif child == 0:\n __import__('time').sleep(0.15)\n raise SystemExit(0)\nopen({:?}, 'w').write(str(child))\n",
            orphan_pid_path
        );
        let mut first_command = Command::new("python3");
        first_command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let first_marker = super::isolate(&mut first_command, super::Isolation::DetachedSession);
        let mut first = super::OwnedChild::new(
            first_command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            first_marker,
        );

        let started = Instant::now();
        let orphan_pid = loop {
            if let Ok(value) = std::fs::read_to_string(&orphan_pid_path) {
                if let Ok(pid) = value.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            let _ = first.exited();
            std::thread::sleep(Duration::from_millis(10));
        };
        while !first.exited().unwrap() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(first.wait().unwrap().success());

        let mut second_command = Command::new("/bin/sleep");
        second_command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let second_marker = super::isolate(&mut second_command, super::Isolation::ExactChild);
        let mut second = super::OwnedChild::new(
            second_command.spawn().unwrap(),
            super::Isolation::ExactChild,
            second_marker,
        );
        let second_pid = second.child.as_ref().unwrap().id();

        let reap_started = Instant::now();
        while std::path::Path::new(&format!("/proc/{orphan_pid}")).exists()
            && reap_started.elapsed() < Duration::from_secs(2)
        {
            let _ = second.exited();
            std::thread::sleep(Duration::from_millis(10));
        }
        let orphan_gone = !std::path::Path::new(&format!("/proc/{orphan_pid}")).exists();
        let second_still_owned = second.child.as_mut().unwrap().try_wait().unwrap().is_none();
        let _ = second.stop(libc::SIGKILL);

        assert!(orphan_gone, "later adopted exit remained as a zombie");
        assert!(
            second_still_owned,
            "reaping an adoptee stole another boundary's leader"
        );
        assert_ne!(orphan_pid, second_pid);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn signal_after_leader_wait_still_cleans_the_retained_boundary() {
        const CHILD_ENV: &str = "SHDEPS_TEST_POST_WAIT_SIGNAL_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::signal_after_leader_wait_still_cleans_the_retained_boundary";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        super::adopt_descendants().unwrap();
        let signals = super::Signals::install_with_restore(true).unwrap();
        let dir = crate::test_support::temp_dir("shdeps-post-wait-signal");
        let descendant_pid_path = dir.join("descendant.pid");
        let _fixture_cleanup = PublishedPidCleanup::new(&descendant_pid_path);
        let release_path = dir.join("release");
        let mutation_path = dir.join("mutations");
        let script = format!(
            "import os, signal, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)]\n start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]\n open({descendant:?}, 'w').write(f'{{os.getpid()}} {{start}}')\n while True:\n  open({mutation:?}, 'a').write('x')\n  time.sleep(0.01)\nwhile not os.path.exists({release:?}):\n time.sleep(0.001)\n",
            descendant = descendant_pid_path,
            mutation = mutation_path,
            release = release_path,
        );
        let mut command = Command::new("python3");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let descendant_pid = loop {
            if let Some((pid, _)) = read_published_identity(&descendant_pid_path) {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            let _ = child.exited();
            std::thread::sleep(Duration::from_millis(10));
        };
        let descendant_pidfd = super::stable_pidfd(super::open_pidfd(descendant_pid).unwrap())
            .unwrap()
            .unwrap();
        child.observe_boundary();
        std::fs::write(&release_path, "exit\n").unwrap();
        while !child.exited().unwrap() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            child.boundary.members.contains_key(&descendant_pid),
            "leader-exit tracking must retain the escaped descendant; members={:?}, descendant={:?}, supervisor_children={:?}, marker={:?}",
            child.boundary.members.keys().collect::<Vec<_>>(),
            super::linux_process_info_checked(descendant_pid),
            super::linux_process_children(
                std::process::id(),
                Instant::now() + Duration::from_secs(1)
            ),
            child
                .boundary
                .marker
                .token
                .as_deref()
                .map(|token| super::process_has_boundary_marker_checked(descendant_pid, token)),
        );

        let waited = child.child.as_mut().unwrap().wait();
        // SAFETY: this isolated process installed the production TERM handler.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let result = child.finish_wait(waited);
        let descendant_survived = super::linux_process_info_checked(descendant_pid)
            .unwrap()
            .is_some_and(|process| process.live);
        if descendant_survived {
            let _ = super::signal_pidfd(&descendant_pidfd, libc::SIGKILL);
        }

        assert!(result.is_err());
        assert!(
            !descendant_survived,
            "post-wait signal leaked the escaped descendant: {result:?}"
        );
        let diagnostics = signals.take_cleanup_diagnostics();
        assert_eq!(
            signals.finish_result::<std::io::Error>(Ok(0)).unwrap(),
            143,
            "unexpected cleanup diagnostics: {diagnostics:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn inferred_interrupt_uses_term_for_post_wait_descendant_cleanup() {
        const CHILD_ENV: &str = "SHDEPS_TEST_POST_WAIT_INT_TERM_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::inferred_interrupt_uses_term_for_post_wait_descendant_cleanup";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        super::adopt_descendants().unwrap();
        let signals = super::Signals::install_with_restore(true).unwrap();
        let dir = crate::test_support::temp_dir("shdeps-post-wait-int-term");
        let descendant_pid_path = dir.join("descendant.pid");
        let _fixture_cleanup = PublishedPidCleanup::new(&descendant_pid_path);
        let release_path = dir.join("release");
        let term_path = dir.join("term");
        let script = format!(
            "import os, signal, time\nchild = os.fork()\nif child == 0:\n os.setsid()\n [signal.signal(s, signal.SIG_IGN) for s in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT)]\n def term(_signal, _frame):\n  open({term:?}, 'w').write('term')\n signal.signal(signal.SIGTERM, term)\n start = open('/proc/self/stat').read().rsplit(')', 1)[1].split()[19]\n open({descendant:?}, 'w').write(f'{{os.getpid()}} {{start}}')\n while True:\n  time.sleep(0.01)\nwhile not os.path.exists({release:?}):\n time.sleep(0.001)\n",
            descendant = descendant_pid_path,
            release = release_path,
            term = term_path,
        );
        let mut command = Command::new("python3");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );
        let started = Instant::now();
        let descendant_pid = loop {
            if let Some((pid, _)) = read_published_identity(&descendant_pid_path) {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            let _ = child.exited();
            std::thread::sleep(Duration::from_millis(10));
        };
        let descendant_pidfd = super::stable_pidfd(super::open_pidfd(descendant_pid).unwrap())
            .unwrap()
            .unwrap();
        child.observe_boundary();
        std::fs::write(&release_path, "exit\n").unwrap();
        while !child.exited().unwrap() {
            std::thread::sleep(Duration::from_millis(10));
        }

        let waited = child.child.as_mut().unwrap().wait();
        // SAFETY: this isolated process installed the production INT handler.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        let result = child.finish_wait(waited);
        let descendant_survived = super::linux_process_info_checked(descendant_pid)
            .unwrap()
            .is_some_and(|process| process.live);
        if descendant_survived {
            let _ = super::signal_pidfd(&descendant_pidfd, libc::SIGKILL);
        }

        assert!(result.is_err());
        assert!(
            term_path.is_file(),
            "cleanup must use TERM even when the inferred final status is SIGINT"
        );
        assert!(!descendant_survived);
        assert_eq!(signals.finish_result::<std::io::Error>(Ok(0)).unwrap(), 130);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn watchdog_cleans_published_fixture_after_parent_sigkill() {
        const CHILD_ENV: &str = "SHDEPS_TEST_WATCHDOG_PARENT_SIGKILL_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::watchdog_cleans_published_fixture_after_parent_sigkill";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }
        super::adopt_descendants().unwrap();
        let dir = crate::test_support::temp_dir("shdeps-watchdog-parent-sigkill");
        let descendant_path = dir.join("descendant.pid");
        let watchdog_path = dir.join("watchdog.pid");
        // The intermediate parent owns the watchdog's stdin pipe and the
        // fixture. SIGKILLing it (no Drop, no cleanup) proves the watchdog
        // observes EOF and delivers exact-identity SIGKILL on its own.
        let intermediate_script = r#"
import subprocess, sys, time
pidfile, watchdog_pidfile, watchdog_script, fixture_script = sys.argv[1:5]
watchdog = subprocess.Popen(
    [sys.executable, "-c", watchdog_script, pidfile],
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
open(watchdog_pidfile, "w").write(str(watchdog.pid))
subprocess.Popen(
    [sys.executable, "-c", fixture_script, pidfile],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)
time.sleep(3600)
"#;
        let fixture_script = r#"
import os, signal, sys, time
for handled in (signal.SIGTERM, signal.SIGINT):
    signal.signal(handled, signal.SIG_IGN)
path = sys.argv[1]
stat = open('/proc/self/stat').read().rsplit(')', 1)[1].split()
open(path, 'w').write(f"{os.getpid()} {stat[19]}")
while True:
    time.sleep(3600)
"#;
        let registration = super::SpawnRegistrationWindow::begin();
        let mut intermediate_command = Command::new("python3");
        intermediate_command
            .args([
                "-c",
                intermediate_script,
                &descendant_path.display().to_string(),
                &watchdog_path.display().to_string(),
                PUBLISHED_PID_WATCHDOG_SCRIPT,
                fixture_script,
            ])
            .env(super::BOUNDARY_ENV, "test-harness-subprocess")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // The harness kills the whole intermediate tree and reaps via raw
        // `waitpid(-1)` with subreaper semantics; `Child::wait` is deliberately
        // unused. The handle is kept only for `id()` during tree discovery.
        #[allow(clippy::zombie_processes)]
        let intermediate = intermediate_command.spawn().unwrap();
        drop(registration);

        let started = std::time::Instant::now();
        let (descendant_pid, descendant_start) = loop {
            if let Some(identity) = read_published_identity(&descendant_path) {
                break identity;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "fixture must publish its exact identity"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let watchdog_pid = loop {
            if let Ok(text) = std::fs::read_to_string(&watchdog_path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "intermediate must publish the watchdog pid"
            );
            std::thread::sleep(Duration::from_millis(5));
        };

        // Both orphans reparent to this subprocess (the subreaper). Reap them
        // by identity and prove the fixture died from SIGKILL (watchdog
        // delivery) while the watchdog exited cleanly after delivering it.
        // The interpreter launcher may stub-fork: the spawn handle can name a
        // waiting stub while the real parent (which owns the watchdog's stdin
        // pipe) runs as its child. Discover the whole intermediate tree and
        // force-kill every member except the watchdog and fixture under test:
        // no Drop, no stdin close, no graceful wait.
        let tree_started = std::time::Instant::now();
        let live_tree = loop {
            let tree = intermediate_tree(intermediate.id());
            if tree.contains(&descendant_pid) && tree.contains(&watchdog_pid) {
                break tree;
            }
            assert!(
                tree_started.elapsed() < Duration::from_secs(5),
                "the intermediate tree must be discoverable"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let kill_set: Vec<u32> = live_tree
            .iter()
            .copied()
            .filter(|pid| *pid != descendant_pid && *pid != watchdog_pid)
            .collect();
        assert!(
            !kill_set.is_empty(),
            "the intermediate tree must contain a parent to kill"
        );
        for pid in &kill_set {
            // SAFETY: kill targets our own live descendants (unreaped, so pids
            // cannot be reused) and SIGKILL takes no handler. ESRCH means the
            // member already exited between scan and kill.
            let killed = unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
            if killed != 0 {
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::ESRCH),
                    "forced parent kill must land or find an exited member: {error}"
                );
            }
        }

        let mut statuses = std::collections::HashMap::new();
        let reap_started = std::time::Instant::now();
        while !kill_set
            .iter()
            .chain(&[descendant_pid, watchdog_pid])
            .all(|pid| statuses.contains_key(pid))
        {
            let mut status = 0;
            // SAFETY: waitpid with WNOHANG reaps this subprocess's children
            // only; the subprocess runs this single test, whose only
            // descendants are the intermediate tree members (the watchdog and
            // fixture reparent here once their parent dies).
            let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if reaped > 0 {
                statuses.insert(reaped as u32, status);
                continue;
            }
            if reaped == -1 {
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::ECHILD),
                    "unexpected waitpid failure: {error}"
                );
                assert!(
                    kill_set
                        .iter()
                        .chain(&[descendant_pid, watchdog_pid])
                        .all(|pid| statuses.contains_key(pid)),
                    "no child may reparent past the subreaper: {statuses:?}"
                );
                break;
            }
            assert!(
                reap_started.elapsed() < Duration::from_secs(5),
                "parents, watchdog, and fixture must all terminate after the parent kill: {statuses:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Drain anything left and prove nothing stray survives.
        loop {
            let mut status = 0;
            let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if reaped > 0 {
                panic!("stray child survived the parent kill: pid {reaped}");
            }
            if reaped == -1 {
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::ECHILD),
                    "unexpected waitpid failure: {error}"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            assert!(
                reap_started.elapsed() < Duration::from_secs(6),
                "reap drain must finish promptly: {statuses:?}"
            );
        }
        assert!(
            kill_set.iter().any(
                |pid| statuses
                    .get(pid)
                    .is_some_and(|status| libc::WIFSIGNALED(*status)
                        && libc::WTERMSIG(*status) == libc::SIGKILL)
            ),
            "the forced kill must land on a live parent: {statuses:?}"
        );
        let descendant_status = statuses.get(&descendant_pid).unwrap_or_else(|| {
            panic!("watchdog must SIGKILL the published fixture after its parent dies: {statuses:?} (fixture {descendant_pid} start {descendant_start})")
        });
        assert!(
            libc::WIFSIGNALED(*descendant_status)
                && libc::WTERMSIG(*descendant_status) == libc::SIGKILL,
            "fixture must die from SIGKILL delivered by the watchdog: {descendant_status}"
        );
        let watchdog_status = statuses
            .get(&watchdog_pid)
            .unwrap_or_else(|| panic!("watchdog must exit after delivering cleanup: {statuses:?}"));
        assert!(
            libc::WIFEXITED(*watchdog_status) && libc::WEXITSTATUS(*watchdog_status) == 0,
            "watchdog must exit cleanly after delivering cleanup: {watchdog_status}"
        );
    }

    #[test]
    fn portable_leader_exit_tracking_runs_at_most_one_snapshot_per_half_second() {
        let now = std::time::Instant::now();
        let loads = Cell::new(0_u32);
        let cache = super::SnapshotCache::default();
        let deadline = now + Duration::from_secs(1);
        let load = |_| {
            loads.set(loads.get() + 1);
            Some(Vec::new())
        };

        let first = cache
            .get_or_load(now, super::PORTABLE_SNAPSHOT_TTL, deadline, load)
            .unwrap();
        let cache_hit = cache
            .get_or_load(
                now + std::time::Duration::from_millis(499),
                super::PORTABLE_SNAPSHOT_TTL,
                deadline,
                load,
            )
            .unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&first, &cache_hit),
            "a cache hit must share the process table instead of cloning every row under the mutex"
        );
        assert_eq!(
            loads.get(),
            1,
            "hot tracking and leader-exit refreshes must share one ps snapshot"
        );
        assert!(
            cache
                .get_or_load(
                    now + std::time::Duration::from_millis(500),
                    Duration::ZERO,
                    deadline,
                    load,
                )
                .is_some()
        );
        assert_eq!(loads.get(), 2, "stale snapshots must be refreshed");
    }

    #[test]
    fn output_supervision_fallback_poll_is_not_a_one_millisecond_spin() {
        assert!(
            super::OUTPUT_POLL >= std::time::Duration::from_millis(10),
            "reader completion should wake supervision; fallback polling need not run at 1 kHz"
        );
    }

    #[test]
    fn cleanup_grace_starts_after_initial_delivery_finishes() {
        let delivered_at = std::cell::Cell::new(None);
        let ((), deadline) = super::delivery_then_deadline(super::GRACE, || {
            // Model an expensive final discovery and a slow signal-delivery
            // path without making correctness depend on scheduler timing.
            std::thread::sleep(std::time::Duration::from_millis(40));
            delivered_at.set(Some(std::time::Instant::now()));
        });
        let delivered_at = delivered_at.get().unwrap();

        assert!(
            deadline.saturating_duration_since(delivered_at) >= super::GRACE,
            "snapshot/delivery work must not consume the TERM-handler grace"
        );
    }

    #[test]
    fn final_kill_verification_starts_after_last_delivery_finishes() {
        let delivered_at = std::cell::Cell::new(None);
        let ((), deadline) = super::final_kill_verification_deadline(|| {
            std::thread::sleep(std::time::Duration::from_millis(40));
            delivered_at.set(Some(std::time::Instant::now()));
        });
        let delivered_at = delivered_at.get().unwrap();

        assert!(
            deadline.saturating_duration_since(delivered_at) >= super::KILL_SETTLE_GRACE,
            "a delivery at the end of discovery must retain a separate stable-empty verification reserve"
        );
    }

    #[test]
    fn grace_empty_counts_first_and_spaced_observations() {
        let spacing = Duration::from_millis(50);
        let mut last = None;
        let first = Instant::now();
        assert!(super::grace_empty_counts(first, &mut last, spacing));
        assert_eq!(last, Some(first));
        assert!(super::grace_empty_counts(
            first + Duration::from_millis(50),
            &mut last,
            spacing
        ));
    }

    #[test]
    fn grace_empty_holds_observations_sharing_one_scan_window() {
        let spacing = Duration::from_millis(50);
        let mut last = None;
        let first = Instant::now();
        assert!(super::grace_empty_counts(first, &mut last, spacing));
        assert!(!super::grace_empty_counts(
            first + Duration::from_millis(20),
            &mut last,
            spacing
        ));
        assert_eq!(
            last,
            Some(first),
            "a held observation must not move the proof window"
        );
    }

    #[test]
    fn grace_empty_with_zero_spacing_counts_every_observation() {
        let mut last = None;
        let first = Instant::now();
        assert!(super::grace_empty_counts(first, &mut last, Duration::ZERO));
        assert!(super::grace_empty_counts(first, &mut last, Duration::ZERO));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn linux_snapshot_cache_shares_rows_without_cloning_the_process_table() {
        let captured = Instant::now();
        let old = super::ProcessInfo {
            pid: 41,
            ppid: 1,
            pgid: 41,
            sid: 41,
            live: true,
            stopped: false,
            identity: super::ProcessIdentity {
                pid: 41,
                start: Some("old".to_owned()),
            },
        };
        let cache = super::SnapshotCache::default();
        *cache.captured.lock().unwrap() = Some((captured, std::sync::Arc::new(vec![old])));
        let loads = Cell::new(0_u32);
        let deadline = captured + Duration::from_secs(1);

        let rows = cache
            .get_or_load(
                captured + Duration::from_millis(2),
                super::LINUX_SNAPSHOT_TTL,
                deadline,
                |_| -> Option<Vec<super::ProcessInfo>> {
                    loads.set(loads.get() + 1);
                    None
                },
            )
            .unwrap();

        assert_eq!(loads.get(), 0, "a fresh whole-system scan should be shared");
        assert_eq!(rows[0].identity.start.as_deref(), Some("old"));
        let reused = cache
            .get_or_load(
                captured + Duration::from_millis(3),
                super::LINUX_SNAPSHOT_TTL,
                deadline,
                |_| panic!("the fresh snapshot should be shared"),
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&rows, &reused));

        let refreshed = cache
            .get_or_load(
                captured + super::LINUX_SNAPSHOT_TTL,
                Duration::ZERO,
                deadline,
                |_| {
                    loads.set(loads.get() + 1);
                    Some(vec![super::ProcessInfo {
                        identity: super::ProcessIdentity {
                            pid: 41,
                            start: Some("fresh".to_owned()),
                        },
                        ..rows[0].clone()
                    }])
                },
            )
            .unwrap();
        assert_eq!(loads.get(), 1);
        assert_eq!(refreshed[0].identity.start.as_deref(), Some("fresh"));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn fresh_snapshot_cache_shares_only_scans_started_after_the_proof_boundary() {
        let proof_boundary = Instant::now();
        let scan_started = proof_boundary + Duration::from_millis(1);
        let cache = super::SnapshotCache::default();
        let rows = std::sync::Arc::new(Vec::new());
        *cache.captured.lock().unwrap() = Some((scan_started, std::sync::Arc::clone(&rows)));
        let deadline = scan_started + Duration::from_secs(1);

        let shared = cache
            .get_or_load(proof_boundary, Duration::ZERO, deadline, |_| {
                panic!("a scan started after this proof boundary must be shared")
            })
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&rows, &shared));

        let loads = Cell::new(0_u32);
        let later_boundary = scan_started + Duration::from_millis(1);
        cache
            .get_or_load(later_boundary, Duration::ZERO, deadline, |_| {
                loads.set(loads.get() + 1);
                Some(Vec::new())
            })
            .unwrap();
        assert_eq!(
            loads.get(),
            1,
            "a scan begun before the relevant proof boundary must not be reused"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn strict_linux_snapshot_rejects_partial_enumeration_and_live_stat_errors() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let partial = super::collect_linux_process_snapshot_with(
            vec![
                Ok(Some(41)),
                Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
                Ok(Some(42)),
            ],
            deadline,
            |_| panic!("process files must not be opened before enumeration completes"),
        );
        assert!(
            partial.is_err(),
            "a partial directory walk is not a snapshot"
        );

        let unreadable =
            super::collect_linux_process_snapshot_with(vec![Ok(Some(41))], deadline, |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected persistent stat failure",
                ))
            });
        // Linux has setuid transitions, so an unreadable stat stays a
        // fail-closed partial view there; Android skips provably foreign
        // rows instead (no setuid).
        #[cfg(not(target_os = "android"))]
        assert!(
            unreadable.is_err(),
            "a live process whose stat cannot be read must fail closed"
        );
        #[cfg(target_os = "android")]
        assert!(
            unreadable.unwrap().is_empty(),
            "a foreign-app denial must skip the row on Android"
        );

        let vanished =
            super::collect_linux_process_snapshot_with(vec![Ok(Some(41))], deadline, |_| Ok(None))
                .unwrap();
        assert!(
            vanished.is_empty(),
            "confirmed ENOENT is a safe disappearance"
        );

        let boundary = super::Boundary::new(
            41,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(Some("strict-snapshot-boundary".to_owned())),
        );
        let partial = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![
                Ok(Some(41)),
                Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
            ],
            deadline,
            |_| panic!("a partial marker scan must not inspect a process"),
            |_, _| panic!("a partial marker scan must not inspect an environment"),
        );
        assert!(
            partial.is_err(),
            "partial marker enumeration must fail before publishing rows"
        );
        let unreadable = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(41))],
            deadline,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected retained-process stat failure",
                ))
            },
            |_, _| Ok(Some(true)),
        );
        assert!(
            unreadable.is_err(),
            "a retained process whose stat is unresolved must fail closed"
        );

        let marker_attempts = Cell::new(0_u32);
        let transient_adoptee = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(42))],
            Instant::now() + Duration::from_secs(1),
            |pid| {
                Ok(Some(super::ProcessInfo {
                    pid,
                    ppid: std::process::id(),
                    pgid: pid,
                    sid: pid,
                    live: true,
                    stopped: false,
                    identity: super::ProcessIdentity {
                        pid,
                        start: Some("adopted".to_owned()),
                    },
                }))
            },
            |_, _| {
                marker_attempts.set(marker_attempts.get() + 1);
                if marker_attempts.get() == 1 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected transient marker read denial",
                    ))
                } else {
                    Ok(Some(false))
                }
            },
        )
        .unwrap();
        assert!(transient_adoptee.is_empty());
        // Linux retries a transient denial through the adoptee path; Android
        // skips the provably foreign entry on the first denial.
        #[cfg(not(target_os = "android"))]
        assert_eq!(marker_attempts.get(), 2);
        #[cfg(target_os = "android")]
        assert_eq!(marker_attempts.get(), 1);

        let unidentifiable_adoptee = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(42))],
            Instant::now() + Duration::from_millis(10),
            |pid| {
                Ok(Some(super::ProcessInfo {
                    pid,
                    ppid: std::process::id(),
                    pgid: pid,
                    sid: pid,
                    live: true,
                    stopped: false,
                    identity: super::ProcessIdentity {
                        pid,
                        start: Some("adopted".to_owned()),
                    },
                }))
            },
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected persistent marker read denial",
                ))
            },
        );
        // Linux has setuid transitions, so an unreadable adoptee marker
        // stays fail-closed there; Android skips provably foreign rows.
        #[cfg(not(target_os = "android"))]
        assert!(
            unidentifiable_adoptee.is_err(),
            "a live adoptee whose marker cannot be read must fail closed"
        );
        #[cfg(target_os = "android")]
        assert!(
            unidentifiable_adoptee.unwrap().is_empty(),
            "a foreign-app denial must skip the row on Android"
        );

        let marker_reads = Cell::new(0_u32);
        let reused = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(43))],
            Instant::now() + Duration::from_secs(1),
            |pid| {
                Ok(Some(super::ProcessInfo {
                    pid,
                    ppid: 1,
                    pgid: pid,
                    sid: pid,
                    live: true,
                    stopped: false,
                    identity: super::ProcessIdentity {
                        pid,
                        start: Some("replacement".to_owned()),
                    },
                }))
            },
            |_, _| {
                marker_reads.set(marker_reads.get() + 1);
                Ok(Some(marker_reads.get() == 1))
            },
        )
        .unwrap();
        assert!(
            reused.is_empty(),
            "a marker that disappears across identity validation must not claim a reused PID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn procfs_foreign_classifies_permission_denied() {
        assert!(super::procfs_process_foreign(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(super::procfs_process_foreign(
            &std::io::Error::from_raw_os_error(libc::EACCES)
        ));
        assert!(!super::procfs_process_foreign(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn snapshot_denied_entry_is_platform_scoped() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = super::collect_linux_process_snapshot_with(
            vec![Ok(Some(123)), Ok(Some(456))],
            deadline,
            |pid| {
                if pid == 456 {
                    return Err(std::io::Error::from_raw_os_error(libc::EACCES));
                }
                Ok(Some(super::ProcessInfo {
                    pid,
                    ppid: 1,
                    pgid: pid,
                    sid: pid,
                    live: true,
                    stopped: false,
                    identity: super::ProcessIdentity {
                        pid,
                        start: Some("100".to_owned()),
                    },
                }))
            },
        );
        #[cfg(target_os = "android")]
        {
            let processes =
                snapshot.expect("a foreign-app denial must skip one row, not fail the snapshot");
            assert_eq!(processes.len(), 1);
            assert_eq!(processes[0].pid, 123);
        }
        // Linux has setuid transitions, so an unreadable stat stays a
        // fail-closed partial view there.
        #[cfg(not(target_os = "android"))]
        assert!(
            snapshot.is_err(),
            "a denied stat must fail the snapshot closed on Linux"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn marker_snapshot_denied_entry_is_platform_scoped() {
        let boundary = super::Boundary::new(
            41,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(Some("denied-marker-boundary".to_owned())),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(43)), Ok(Some(44))],
            deadline,
            |pid| {
                if pid == 44 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected foreign stat denial",
                    ));
                }
                Ok(Some(super::ProcessInfo {
                    pid,
                    ppid: 1,
                    pgid: pid,
                    sid: pid,
                    live: true,
                    stopped: false,
                    identity: super::ProcessIdentity {
                        pid,
                        start: Some("marked".to_owned()),
                    },
                }))
            },
            |pid, _| {
                if pid == 44 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected foreign environ denial",
                    ));
                }
                Ok(Some(true))
            },
        );
        #[cfg(target_os = "android")]
        {
            let processes =
                snapshot.expect("a foreign-app denial must skip one row, not fail the snapshot");
            assert_eq!(processes.len(), 1);
            assert_eq!(processes[0].pid, 43);
        }
        // Linux has setuid transitions, so an unreadable stat stays a
        // fail-closed partial view there.
        #[cfg(not(target_os = "android"))]
        assert!(
            snapshot.is_err(),
            "a denied stat must fail the snapshot closed on Linux"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn inaccessible_adoptee_registered_to_another_boundary_is_not_claimed() {
        let identity = super::ProcessIdentity {
            pid: u32::MAX - 41,
            start: Some("other-boundary-generation".to_owned()),
        };
        let mut first = super::Boundary::new(
            u32::MAX - 42,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(Some("first-test-boundary".to_owned())),
        );
        let second = super::Boundary::new(
            u32::MAX - 43,
            super::Isolation::ExactChild,
            super::BoundaryMarker::without_lifetime(Some("second-test-boundary".to_owned())),
        );
        first.retain_identity(identity.clone());

        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            first
                .registered_marker_disposition(&identity, deadline)
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            second
                .registered_marker_disposition(&identity, deadline)
                .unwrap(),
            Some(false),
            "one boundary must not claim an inaccessible child registered to another"
        );
        drop(first);
        assert_eq!(
            second
                .registered_marker_disposition(&identity, deadline)
                .unwrap(),
            None
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn procfs_dead_transition_is_not_treated_as_a_live_process() {
        let pid = std::process::id();
        let mut stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
        let state = stat
            .windows(2)
            .rposition(|part| part == b") ")
            .map(|end| end + 2)
            .unwrap();
        stat[state] = b'X';

        let process = super::parse_linux_process_stat(pid, &stat).unwrap();
        assert!(!process.live);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn lineage_discovery_finds_an_escape_after_a_complete_pid_wrap() {
        // An endpoint-only cursor sees no movement after a complete wrap plus
        // a suffix that lands on the same numeric PID. Ownership discovery
        // must therefore use retained lineage/marker evidence instead.
        let endpoint_before = 41_u32;
        let endpoint_after_full_wrap = 41_u32;
        assert_eq!(endpoint_before, endpoint_after_full_wrap);

        let process = |pid, ppid, pgid, sid, live, start: &str| super::ProcessInfo {
            pid,
            ppid,
            pgid,
            sid,
            live,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(start.to_owned()),
            },
        };
        let leader = process(41, 1, 41, 41, true, "leader");
        let escaped = process(7, leader.pid, 7, 7, true, "escaped");
        let boundary = super::Boundary::new(
            leader.pid,
            super::Isolation::DetachedSession,
            super::BoundaryMarker::without_lifetime(Some("full-wrap-boundary".to_owned())),
        );
        let rows = [leader.clone(), escaped.clone()]
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();

        let owned = super::linux_owned_process_snapshot_with(
            &boundary,
            Instant::now() + Duration::from_secs(1),
            |pid| Ok(rows.get(&pid).cloned()),
            |pid, _| {
                Ok(Some(if pid == leader.pid {
                    vec![escaped.pid]
                } else {
                    Vec::new()
                }))
            },
        )
        .unwrap();

        assert!(
            owned
                .iter()
                .any(|process| process.identity == escaped.identity),
            "local child traversal must not depend on PID cursor movement"
        );

        let adopted = process(
            escaped.pid,
            std::process::id(),
            escaped.pgid,
            escaped.sid,
            true,
            "escaped",
        );
        let rows = [leader, adopted.clone()]
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();
        let reconciled = super::collect_linux_boundary_marker_snapshot_with(
            &boundary,
            vec![Ok(Some(endpoint_after_full_wrap)), Ok(Some(adopted.pid))],
            Instant::now() + Duration::from_secs(1),
            |pid| Ok(rows.get(&pid).cloned()),
            |pid, token| {
                assert_eq!(token, "full-wrap-boundary");
                Ok(Some(pid == adopted.pid))
            },
        )
        .unwrap();
        assert!(
            reconciled
                .iter()
                .any(|process| process.identity == adopted.identity),
            "fresh marker reconciliation must inspect an escaped PID outside the apparent cursor interval"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn leader_exit_discovery_includes_a_supervisor_reparented_escape() {
        // A leader that forks and exits between observations reparents its
        // escape to this supervisor, where leader-descended traversal cannot
        // see it. The leader-exit snapshot must merge supervisor-adopted
        // candidates so the unmarked-adoptee policy can attribute them (sole
        // boundary) or fail closed (ambiguous) instead of publishing a false
        // empty proof.
        let supervisor = std::process::id();
        // SAFETY: getsid observes our own session without pointers.
        let own_sid = unsafe { libc::getsid(0) } as u32;
        let process = |pid, ppid, pgid, sid, live, start: &str| super::ProcessInfo {
            pid,
            ppid,
            pgid,
            sid,
            live,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(start.to_owned()),
            },
        };
        let leader = process(41, supervisor, 41, own_sid.wrapping_add(2), false, "leader");
        let escaped = process(7, supervisor, 7, own_sid.wrapping_add(3), true, "escaped");
        let rows = [leader.clone(), escaped.clone()]
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();
        let deadline = Instant::now() + Duration::from_secs(1);

        let merged = super::linux_local_snapshot_with_supervisor_children(
            vec![leader.clone()],
            deadline,
            |_| Ok(Some(vec![leader.pid, escaped.pid])),
            |pid| Ok(rows.get(&pid).cloned()),
        )
        .unwrap();
        assert!(
            merged
                .iter()
                .any(|process| process.identity == escaped.identity),
            "leader-exit discovery must see an escape reparented to the supervisor"
        );
        assert_eq!(
            merged
                .iter()
                .filter(|process| process.pid == leader.pid)
                .count(),
            1,
            "supervisor children already under observation must not duplicate rows"
        );

        let vanished = super::linux_local_snapshot_with_supervisor_children(
            vec![leader.clone()],
            deadline,
            |_| Ok(None),
            |pid| Ok(rows.get(&pid).cloned()),
        )
        .unwrap();
        assert_eq!(
            vanished.len(),
            1,
            "an unreadable supervisor child list must not invent rows"
        );

        super::linux_local_snapshot_with_supervisor_children(
            vec![leader],
            deadline,
            |_| Err(std::io::Error::other("children unavailable")),
            |pid| Ok(rows.get(&pid).cloned()),
        )
        .expect_err("a failed supervisor child listing must fail the snapshot closed");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn leader_exit_discovery_ignores_same_session_supervisor_children() {
        // A direct child that shares the supervisor session is
        // indistinguishable from a plain supervisor spawn: only setsid moves
        // a process out of its inherited session. Surfacing same-session
        // children lets one test's live fixtures fail another test's
        // leader-exit observation under parallel nextest shards. Only
        // detached (different-session) children are escapee-shaped and may
        // reach the unmarked-adoptee policy.
        let supervisor = std::process::id();
        // SAFETY: getsid observes our own session without pointers.
        let own_sid = unsafe { libc::getsid(0) } as u32;
        let process = |pid, ppid, sid, live, start: &str| super::ProcessInfo {
            pid,
            ppid,
            pgid: pid,
            sid,
            live,
            stopped: false,
            identity: super::ProcessIdentity {
                pid,
                start: Some(start.to_owned()),
            },
        };
        let leader = process(41, supervisor, 41, false, "leader");
        let plain = process(8, supervisor, own_sid, true, "plain");
        let detached = process(7, supervisor, own_sid.wrapping_add(1), true, "detached");
        let rows = [leader.clone(), plain.clone(), detached.clone()]
            .into_iter()
            .map(|process| (process.pid, process))
            .collect::<std::collections::BTreeMap<_, _>>();
        let deadline = Instant::now() + Duration::from_secs(1);

        let merged = super::linux_local_snapshot_with_supervisor_children(
            vec![leader.clone()],
            deadline,
            |_| Ok(Some(vec![leader.pid, plain.pid, detached.pid])),
            |pid| Ok(rows.get(&pid).cloned()),
        )
        .unwrap();
        assert!(
            !merged
                .iter()
                .any(|process| process.identity == plain.identity),
            "a same-session supervisor child must not reach the adoptee policy"
        );
        assert!(
            merged
                .iter()
                .any(|process| process.identity == detached.identity),
            "a detached supervisor child must stay visible to the adoptee policy"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn blocked_snapshot_loader_does_not_hold_the_cache_lock_or_outlive_a_waiter_deadline() {
        let cache = std::sync::Arc::new(super::SnapshotCache::default());
        let (started_sender, started) = std::sync::mpsc::channel();
        let (release_sender, release) = std::sync::mpsc::channel();
        let loader_cache = std::sync::Arc::clone(&cache);
        let loader = std::thread::spawn(move || {
            let now = Instant::now();
            loader_cache.get_or_load(
                now,
                super::LINUX_SNAPSHOT_TTL,
                now + Duration::from_secs(2),
                |_| {
                    assert!(
                        loader_cache.captured.try_lock().is_ok(),
                        "the potentially blocking loader must run outside the cache mutex"
                    );
                    started_sender.send(()).unwrap();
                    release.recv().unwrap();
                    Some(Vec::new())
                },
            )
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();

        let started_wait = Instant::now();
        let deadline = started_wait + Duration::from_millis(30);
        let waited = cache.get_or_load(started_wait, super::LINUX_SNAPSHOT_TTL, deadline, |_| {
            panic!("a second loader must not start while the first is active")
        });
        assert!(waited.is_none());
        assert!(
            started_wait.elapsed() < Duration::from_millis(150),
            "a cache waiter must honor its own deadline"
        );

        release_sender.send(()).unwrap();
        assert!(loader.join().unwrap().is_some());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn successful_local_discovery_avoids_global_scans_for_many_boundaries() {
        let global_scans = Cell::new(0_u32);
        for _ in 0..64 {
            let rows = super::linux_local_or_full_snapshot(
                || Ok(Vec::new()),
                || {
                    global_scans.set(global_scans.get() + 1);
                    Some(std::sync::Arc::new(Vec::new()))
                },
            )
            .unwrap();
            assert!(rows.0.is_empty());
        }
        assert_eq!(
            global_scans.get(),
            0,
            "normal boundary polling must not enumerate all of procfs"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn missing_task_children_interface_is_not_an_empty_child_set() {
        let proc_root = crate::test_support::temp_dir("shdeps-missing-task-children");
        let pid = 42_u32;
        std::fs::create_dir_all(proc_root.join(pid.to_string()).join("task/42")).unwrap();

        let error = super::linux_process_children_at(
            &proc_root,
            pid,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }

    #[cfg(unix)]
    #[test]
    fn open_lifetime_lease_prevents_an_empty_cleanup_proof() {
        const CHILD_ENV: &str = "SHDEPS_TEST_OPEN_LIFETIME_LEASE_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::open_lifetime_lease_prevents_an_empty_cleanup_proof";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let signals = super::Signals::install_with_restore(true).unwrap();
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let retained_writer = writer.try_clone().unwrap();
        let marker = super::BoundaryMarker {
            token: Some("live-unattributed-descendant".to_owned()),
            lifetime_reader: Some(reader),
            lifetime_writer: Some(writer),
        };
        let mut boundary =
            super::Boundary::new(u32::MAX - 101, super::Isolation::ExactChild, marker);

        let error = boundary
            .verify_empty_observation(true, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert!(error.to_string().contains("ownership descriptor"));
        super::record_cleanup_error(&error);

        drop(retained_writer);
        assert!(
            boundary
                .verify_empty_observation(true, Instant::now() + Duration::from_secs(1))
                .unwrap(),
            "EOF on the private descriptor permits the process observation to prove emptiness"
        );
        assert!(
            !boundary
                .verify_empty_observation(false, Instant::now() + Duration::from_secs(1))
                .unwrap(),
            "a non-empty process observation remains non-empty after the lease closes"
        );
        // SAFETY: this isolated process installed the production TERM handler.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        assert!(
            signals
                .take_cleanup_diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.contains("ownership descriptor"))
        );
        assert_eq!(
            signals.finish_result::<std::io::Error>(Ok(0)).unwrap(),
            1,
            "an unaccounted open lease must not be acknowledged as 128+signal"
        );
    }

    // A concurrent spawn can fork while a boundary's writer is still open in
    // the parent; gated fixture children linger pre-exec behind release
    // sockets, so the inherited lease stays open after the owner's leader
    // exits. The empty proof must settle that transient cohort instead of
    // failing closed on a holder that is already on its way out.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn empty_proof_settles_a_transient_pre_exec_inheritor() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        // Ready gate: the lingering child signals before it starts its
        // hold so the first lease read deterministically observes a
        // holder (without the settle, this test fails).
        let (ready_reader, ready_writer) = UnixStream::pair().unwrap();
        let ready_fd = ready_writer.as_raw_fd();
        // SAFETY: the child only uses async-signal-safe calls (write,
        // nanosleep, _exit) before exiting; it never returns to Rust code.
        let holder = unsafe { libc::fork() };
        assert!(holder >= 0, "fork failed");
        if holder == 0 {
            unsafe {
                let byte = [1_u8];
                let _ = libc::write(ready_fd, byte.as_ptr().cast(), byte.len());
                let hold = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 200_000_000,
                };
                libc::nanosleep(&hold, std::ptr::null_mut());
                libc::_exit(0);
            }
        }
        let marker = super::BoundaryMarker {
            token: Some("transient-pre-exec-inheritor".to_owned()),
            lifetime_reader: Some(reader),
            lifetime_writer: Some(writer),
        };
        let mut boundary =
            super::Boundary::new(u32::MAX - 102, super::Isolation::ExactChild, marker);
        let mut ready = [0_u8; 1];
        use std::io::Read as _;
        (&ready_reader).read_exact(&mut ready).unwrap();
        // Hold a registration for the cohort the settle waits on, mirroring
        // a spawn whose fork-child is still pre-exec, and release it when
        // the holder exits.
        let (registered_tx, registered_rx) = std::sync::mpsc::channel();
        let reaper = std::thread::spawn(move || {
            let _registration = super::SpawnRegistrationWindow::begin();
            registered_tx.send(()).unwrap();
            let mut status = 0;
            // SAFETY: holder is the direct child forked above.
            unsafe {
                libc::waitpid(holder, &mut status, 0);
            }
        });
        registered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            boundary
                .verify_empty_observation(true, Instant::now() + Duration::from_secs(5))
                .unwrap(),
            "a pre-exec inheritor that exits must not fail the empty proof"
        );
        reaper.join().unwrap();
    }

    // The settle above is bounded: a lease that is still open once the
    // captured spawn cohort drains (or the caller deadline expires) is a
    // genuine leak and must still fail closed.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn empty_proof_still_fails_closed_when_the_settle_expires() {
        use std::os::unix::net::UnixStream;

        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        // SAFETY: the child only uses async-signal-safe calls (nanosleep,
        // _exit) before exiting; it never returns to Rust code.
        let holder = unsafe { libc::fork() };
        assert!(holder >= 0, "fork failed");
        if holder == 0 {
            unsafe {
                let hold = libc::timespec {
                    tv_sec: 30,
                    tv_nsec: 0,
                };
                libc::nanosleep(&hold, std::ptr::null_mut());
                libc::_exit(0);
            }
        }
        let marker = super::BoundaryMarker {
            token: Some("stuck-pre-exec-inheritor".to_owned()),
            lifetime_reader: Some(reader),
            lifetime_writer: Some(writer),
        };
        let mut boundary =
            super::Boundary::new(u32::MAX - 103, super::Isolation::ExactChild, marker);
        // Mirror a spawn stuck behind its release gate: the registration
        // outlives the caller deadline exactly like the holder does.
        let _registration = super::SpawnRegistrationWindow::begin();
        let started = Instant::now();
        let error = boundary
            .verify_empty_observation(true, started + Duration::from_millis(200))
            .unwrap_err();
        assert!(
            error.to_string().contains("ownership descriptor"),
            "unexpected settle-expiry error: {error}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the settle must wait out the cohort before failing closed"
        );
        // SAFETY: holder is the direct child forked above; SIGKILL ends the
        // hold so the test reaps a finite child.
        unsafe {
            libc::kill(holder, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(holder, &mut status, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawned_child_inherits_the_lifetime_lease_until_exit() {
        const CHILD_ENV: &str = "SHDEPS_TEST_LIFETIME_LEASE_EXEC_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::spawned_child_inherits_the_lifetime_lease_until_exit";
        if std::env::var_os(CHILD_ENV).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST_NAME, "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let marker = super::isolate(&mut command, super::Isolation::DetachedSession);
        let mut child = super::OwnedChild::new(
            command.spawn().unwrap(),
            super::Isolation::DetachedSession,
            marker,
        );

        assert_eq!(
            child.boundary.lifetime_has_holders().unwrap(),
            Some(true),
            "the child-side pre-exec hook must preserve the private descriptor across exec"
        );
        child.stop(libc::SIGKILL).unwrap();
        assert_eq!(
            child.boundary.lifetime_has_holders().unwrap(),
            Some(false),
            "reaping the complete subprocess boundary must close the inherited descriptor"
        );
    }

    #[test]
    fn incomplete_cleanup_is_not_acknowledged_as_normal_signal_completion() {
        const CHILD_ENV: &str = "SHDEPS_TEST_CLEANUP_DIAGNOSTIC_CHILD";
        const TEST_NAME: &str = "cancellation::tests::incomplete_cleanup_is_not_acknowledged_as_normal_signal_completion";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let signals = super::Signals::install_with_restore(true).unwrap();
        super::record_cleanup_error(&std::io::Error::other("SIGKILL delivery failed"));
        super::record_cleanup_error(&std::io::Error::other("SIGKILL delivery failed"));
        super::record_cleanup_error(&std::io::Error::other("owned descendant survived"));
        // SAFETY: this isolated process installed the production TERM handler.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);

        assert_eq!(
            signals.take_cleanup_diagnostics(),
            vec![
                "SIGKILL delivery failed".to_owned(),
                "owned descendant survived".to_owned(),
            ],
            "cleanup failures must remain visible to the CLI"
        );
        assert_eq!(
            signals.finish_result::<std::io::Error>(Ok(0)).unwrap(),
            1,
            "incomplete cleanup must not masquerade as a conventional signal acknowledgement"
        );
    }

    #[test]
    fn reader_errors_panics_and_stalls_are_recorded_as_cleanup_failures() {
        const CHILD_ENV: &str = "SHDEPS_TEST_READER_FAILURE_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::reader_errors_panics_and_stalls_are_recorded_as_cleanup_failures";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let signals = super::Signals::install_with_restore(true).unwrap();
        let read_error = std::thread::spawn(|| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("injected read failure"))
        });
        assert!(super::join_output_reader(read_error, "stdout").is_err());
        let panic =
            std::thread::spawn(|| -> std::io::Result<Vec<u8>> { panic!("injected reader panic") });
        assert!(super::join_output_reader(panic, "stderr").is_err());
        assert!(super::unfinished_output_reader("stdout").is_err());
        let diagnostics = signals.take_cleanup_diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("injected read failure"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("stderr output reader panicked"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("stdout output reader did not finish"))
        );
        // SAFETY: this isolated process installed the production TERM handler.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        assert_eq!(
            signals.finish_result::<std::io::Error>(Ok(0)).unwrap(),
            1,
            "a bounded drain failure must not acknowledge signal cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonblocking_stdin_write_reports_ordinary_errors_without_detaching_a_writer() {
        struct ScriptedWriter {
            calls: usize,
        }

        impl std::io::Write for ScriptedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(bytes.len().min(2)),
                    2 => Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
                    _ => Err(std::io::Error::other("injected stdin write failure")),
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = ScriptedWriter { calls: 0 };
        let mut written = 0;
        assert!(!super::write_available(&mut writer, b"input", &mut written).unwrap());
        assert_eq!(written, 2, "partial progress is retained across polls");
        let error = super::write_available(&mut writer, b"input", &mut written).unwrap_err();
        assert_eq!(error.to_string(), "injected stdin write failure");
        assert_eq!(
            written, 2,
            "a write error cannot be mistaken for completion"
        );
    }

    #[cfg(unix)]
    #[test]
    fn portable_snapshot_spawn_guard_obeys_the_snapshot_deadline() {
        let root = crate::test_support::temp_dir("portable-snapshot-spawn-deadline");
        let side_effect = root.join("spawned");
        let lease = super::exclusive_spawn_guard(Instant::now() + Duration::from_secs(1)).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "printf spawned >\"$1\"", "sh"])
                    .arg(&side_effect);
                started_tx.send(()).unwrap();
                let output = super::snapshot(command, Instant::now() + Duration::from_millis(40));
                finished_tx.send(output).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let bounded = finished_rx.recv_timeout(Duration::from_millis(250));
            drop(lease);
            let output = match bounded {
                Ok(output) => output,
                Err(error) => {
                    let _ = finished_rx.recv_timeout(Duration::from_secs(1));
                    panic!("snapshot waited beyond its deadline for the spawn lease: {error}");
                }
            };
            assert!(output.is_none());
        });

        assert!(!side_effect.exists(), "a timed-out snapshot helper spawned");
    }

    #[cfg(unix)]
    #[test]
    fn portable_snapshot_spawn_survives_a_received_signal() {
        const CHILD_ENV: &str = "SHDEPS_TEST_SNAPSHOT_AFTER_SIGNAL_CHILD";
        const TEST_NAME: &str =
            "cancellation::tests::portable_snapshot_spawn_survives_a_received_signal";
        if std::env::var_os(CHILD_ENV).is_none() {
            crate::test_support::run_signal_boundary_subprocess(TEST_NAME, CHILD_ENV);
            return;
        }

        let _signals = super::Signals::install().unwrap();
        // SAFETY: the handler is installed above and targets this process only.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        assert_eq!(super::received_signal(), Some(libc::SIGTERM));
        // Teardown snapshots observe owned subprocesses during cancellation;
        // refusing their helper spawn fails the cleanup proof on platforms
        // without procfs (macOS) and misreports 1 instead of 128+signal.
        let output = super::snapshot(
            Command::new("true"),
            Instant::now() + Duration::from_secs(5),
        );
        assert!(output.is_some(), "snapshot helper refused after signal");
    }

    #[cfg(unix)]
    #[test]
    fn portable_snapshot_spawn_waits_for_the_exclusive_prompt_lease() {
        let root = crate::test_support::temp_dir("portable-snapshot-spawn-lease");
        let side_effect = root.join("spawned");
        let lease = super::exclusive_spawn_guard(Instant::now() + Duration::from_secs(1)).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "printf spawned >\"$1\"", "sh"])
                    .arg(&side_effect);
                started_tx.send(()).unwrap();
                let output = super::snapshot(command, Instant::now() + Duration::from_secs(2));
                finished_tx.send(output).unwrap();
            });

            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(
                finished_rx.recv_timeout(Duration::from_millis(25)).is_err(),
                "the portable snapshot helper bypassed the exclusive prompt-reader lease"
            );
            assert!(!side_effect.exists());
            drop(lease);
            assert!(
                finished_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .is_some(),
                "the portable snapshot helper did not proceed after the lease closed"
            );
        });

        assert_eq!(std::fs::read_to_string(side_effect).unwrap(), "spawned");
    }

    #[cfg(unix)]
    #[test]
    fn portable_snapshot_stops_enrichment_when_deadline_expires() {
        let lookups = Cell::new(0_u32);
        let checks = Cell::new(0_u32);
        let snapshot = super::parse_ps_processes(
            "10 1 S Mon Sep 8 12:00:00 2026\n11 10 S Mon Sep 8 12:00:01 2026\n",
            || {
                let count = checks.get() + 1;
                checks.set(count);
                count >= 4
            },
            |pid| {
                lookups.set(lookups.get() + 1);
                Some((pid, 1, None))
            },
        );

        assert!(snapshot.is_none(), "expired snapshot must fail closed");
        assert_eq!(
            lookups.get(),
            1,
            "deadline must be checked before enriching the next process row"
        );
    }

    #[cfg(unix)]
    #[test]
    fn portable_snapshot_rejects_malformed_nonblank_rows() {
        let snapshot = super::parse_ps_processes(
            "10 1 Z Mon Sep 8 12:00:00 2026\n11 10\n",
            || false,
            |pid| Some((pid, 1, Some(format!("start-{pid}")))),
        );

        assert!(
            snapshot.is_none(),
            "a truncated child row after a dead leader must invalidate the snapshot"
        );
    }

    #[cfg(target_os = "android")]
    #[test]
    fn termux_real_shell_descendant_is_cancelled_and_drained() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let root = crate::test_support::temp_dir("termux-cancellation");
        let pid_path = root.join("descendant.pid");
        let mut signals = super::Signals::install_with_restore(true).unwrap();
        super::adopt_descendants().unwrap();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                "sh -c 'trap \"\" HUP INT QUIT TERM; printf \"%s\\n\" \"$$\" >\"$SHDEPS_TEST_PID\"; while :; do sleep 1; done' & exit 0",
            )
            .env("SHDEPS_TEST_PID", &pid_path);
        let signaler = std::thread::spawn({
            let pid_path = pid_path.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !std::fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .is_some_and(|pid| pid > 0)
                {
                    assert!(Instant::now() < deadline, "Termux descendant did not start");
                    std::thread::sleep(Duration::from_millis(10));
                }
                // SAFETY: target is this test process with its handler installed.
                assert_eq!(
                    unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) },
                    0
                );
            }
        });

        let result = super::output(command, None);
        signaler.join().unwrap();
        assert!(result.is_err());
        assert_eq!(signals.close(), Some(libc::SIGTERM));
    }
}
