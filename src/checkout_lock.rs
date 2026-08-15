//! Shared checkout mutation-lock protocol primitives.
//!
//! The generated checkout installer and Shdeps can both update the same
//! `github:repo` root. Their lock is therefore a wire protocol rather than an
//! implementation detail. This module starts with the exact parser and wire
//! transformations so later filesystem arbitration is built on bytes already
//! proven compatible with the Actions-owned conformance fixtures.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

const HEADER: &str = "cgraf78 checkout mutation lock v1";
const OWNER_ROLE_HEX: &str = "6f776e6572";
const CLAIM_ROLE_HEX: &str = "636c61696d";
const PROC_STAT_KIND_HEX: &str = "70726f632d73746174";
const PS_LSTART_KIND_HEX: &str = "70732d6c7374617274";
const TIMEOUT_ENV: &str = "SHDEPS_CHECKOUT_LOCK_TIMEOUT_SECS";
const DEFAULT_TIMEOUT_SECS: u64 = 1_800;
const WAIT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Owner,
    Claim,
}

impl Role {
    // Return the exact lowercase-hex role value committed to the v1 wire format.
    fn wire_hex(self) -> &'static str {
        match self {
            Self::Owner => OWNER_ROLE_HEX,
            Self::Claim => CLAIM_ROLE_HEX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    role: Role,
    nonce: String,
    owner_nonce: Option<String>,
    pid: String,
    host_hex: String,
    start_kind_hex: String,
    start_token_hex: String,
    checkout_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessIdentity {
    host_hex: String,
    start_kind_hex: String,
    start_token_hex: String,
    state: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Live,
    Dead,
    Unknown,
}

#[derive(Debug, Clone)]
struct Paths {
    checkout: PathBuf,
    parent: PathBuf,
    name: String,
    canonical: PathBuf,
}

impl Paths {
    // Resolve the parent physically so every writer locks and mutates one spelling.
    fn new(requested_checkout: &Path) -> io::Result<Self> {
        if !normalized_absolute_path(requested_checkout) {
            return Err(invalid_input(format!(
                "managed checkout path must be normalized and absolute: {}",
                requested_checkout.display()
            )));
        }
        let name = checkout_basename(requested_checkout)?;
        let requested_parent = requested_checkout
            .parent()
            .ok_or_else(|| invalid_input("managed checkout path has no parent"))?;
        fs::create_dir_all(requested_parent)?;
        let parent = fs::canonicalize(requested_parent)?;
        if !fs::metadata(&parent)?.is_dir() {
            return Err(invalid_input(format!(
                "managed checkout parent is not a directory: {}",
                parent.display()
            )));
        }
        let checkout = parent.join(&name);
        let canonical = parent.join(format!(".{name}.install.lock"));
        Ok(Self {
            checkout,
            parent,
            name,
            canonical,
        })
    }

    // Return the final private directory for one owner generation.
    fn owner_dir(&self, nonce: &str) -> PathBuf {
        self.parent
            .join(format!(".{}.install.lock.owner.{nonce}", self.name))
    }

    // Return the exact portable relative symlink target for one generation.
    fn owner_target(&self, nonce: &str) -> String {
        canonical_target(&self.name, nonce).expect("validated nonce")
    }

    // Return the final private claim directory binding owner and claimant.
    fn claim_dir(&self, owner_nonce: &str, claimant_nonce: &str) -> PathBuf {
        self.parent.join(format!(
            ".{}.install.lock.claim.{owner_nonce}.{claimant_nonce}",
            self.name
        ))
    }

    // Return the prefix used to discover claims for one owner generation.
    fn claim_prefix(&self, owner_nonce: &str) -> String {
        format!(".{}.install.lock.claim.{owner_nonce}.", self.name)
    }
}

// Keep the protocol's UTF-8 basename grammar in one place so caller-path
// normalization cannot mutate a path that strict acquisition would reject.
fn checkout_basename(path: &Path) -> io::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && !name.contains(['\n', '\r', '/']))
        .map(str::to_owned)
        .ok_or_else(|| invalid_input("managed checkout path has no safe basename"))
}

// Validate raw path bytes before Rust's component iterator normalizes them away.
fn normalized_absolute_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes.first() != Some(&b'/')
        || bytes.last() == Some(&b'/')
        || bytes.contains(&b'\n')
        || bytes.contains(&b'\r')
    {
        return false;
    }
    bytes[1..]
        .split(|byte| *byte == b'/')
        .all(|segment| !segment.is_empty() && segment != b"." && segment != b"..")
}

#[derive(Debug, Clone)]
struct Owner {
    nonce: String,
    target: String,
    dir: PathBuf,
}

#[derive(Debug, Clone)]
struct Claim {
    nonce: String,
    dir: PathBuf,
}

#[derive(Debug, Clone)]
enum Classified {
    Missing,
    Legacy,
    Owner {
        liveness: Liveness,
        pid: String,
        owner: Owner,
    },
    Claim {
        liveness: Liveness,
        pid: String,
        owner_nonce: String,
        owner_target: String,
        claim_dir: PathBuf,
    },
    Retry,
    Malformed(String),
}

#[derive(Debug)]
struct CheckoutLock {
    paths: Paths,
    owner: Option<Owner>,
}

impl CheckoutLock {
    // Acquire one v1 generation, recovering only owners proven dead.
    fn acquire(requested_checkout: &Path, timeout: Duration) -> io::Result<Self> {
        let paths = Paths::new(requested_checkout)?;
        acquire(&paths, timeout)
    }

    // Release through the same ownership-transfer state machine used by recovery.
    fn release(&mut self) -> io::Result<()> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        if let Err(error) = release_owner(&self.paths, &owner) {
            self.owner = Some(owner);
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for CheckoutLock {
    // Best-effort unwinding cleanup complements, but never replaces, strict release.
    fn drop(&mut self) {
        let _ = self.release();
    }
}

// Encode raw bytes directly because checkout paths need not be Unicode.
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// Return the exact checkout identity used by both protocol implementations.
fn checkout_hex(checkout: &Path) -> String {
    #[cfg(unix)]
    {
        encode_hex(checkout.as_os_str().as_bytes())
    }

    #[cfg(not(unix))]
    {
        encode_hex(checkout.as_os_str().to_string_lossy().as_bytes())
    }
}

// Validate one bounded, nonempty lowercase even-length hex field.
fn valid_hex(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.len() % 2 == 0
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

// Validate the 128-bit generation identifier used in path and record names.
fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// Validate a positive decimal PID without imposing a host-sized integer bound.
fn valid_pid(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|first| (b'1'..=b'9').contains(first))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

// Parse one fixed-order record after checking its exact raw-byte framing.
fn parse_record(
    bytes: &[u8],
    expected_role: Role,
    expected_nonce: &str,
    expected_owner_nonce: Option<&str>,
    checkout: &Path,
) -> Result<Record, &'static str> {
    if bytes.contains(&0) || !bytes.ends_with(b"\n") {
        return Err("record framing is invalid");
    }

    let lines = bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    if lines.len() != 9 {
        return Err("record must contain exactly nine lines");
    }
    if lines[0] != HEADER.as_bytes() {
        return Err("record header is invalid");
    }

    let role = field(lines[1], b"role=")?;
    let nonce = field(lines[2], b"nonce=")?;
    let owner_nonce = field(lines[3], b"owner_nonce=")?;
    let pid = field(lines[4], b"pid=")?;
    let host_hex = field(lines[5], b"host_hex=")?;
    let start_kind_hex = field(lines[6], b"start_kind_hex=")?;
    let start_token_hex = field(lines[7], b"start_token_hex=")?;
    let parsed_checkout_hex = field(lines[8], b"checkout_hex=")?;

    if role != expected_role.wire_hex()
        || nonce != expected_nonce
        || owner_nonce != expected_owner_nonce.unwrap_or_default()
    {
        return Err("record identity does not match its path");
    }
    if !valid_nonce(nonce) || !valid_pid(pid) {
        return Err("record nonce or pid is invalid");
    }
    match expected_role {
        Role::Owner if !owner_nonce.is_empty() => {
            return Err("owner records cannot name an owner nonce");
        }
        Role::Claim if !valid_nonce(owner_nonce) => {
            return Err("claim records require a valid owner nonce");
        }
        _ => {}
    }
    if !valid_hex(host_hex, 512) || !valid_hex(start_token_hex, 2048) {
        return Err("record host or process token is invalid");
    }
    if start_kind_hex != PROC_STAT_KIND_HEX && start_kind_hex != PS_LSTART_KIND_HEX {
        return Err("record process backend is unsupported");
    }
    if !valid_hex(parsed_checkout_hex, 8192) || parsed_checkout_hex != checkout_hex(checkout) {
        return Err("record checkout identity is invalid");
    }

    Ok(Record {
        role: expected_role,
        nonce: nonce.to_owned(),
        owner_nonce: (!owner_nonce.is_empty()).then(|| owner_nonce.to_owned()),
        pid: pid.to_owned(),
        host_hex: host_hex.to_owned(),
        start_kind_hex: start_kind_hex.to_owned(),
        start_token_hex: start_token_hex.to_owned(),
        checkout_hex: parsed_checkout_hex.to_owned(),
    })
}

// Extract one fixed field without accepting reordered or duplicate keys.
fn field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a str, &'static str> {
    let value = line
        .strip_prefix(prefix)
        .ok_or("record field is missing or reordered")?;
    std::str::from_utf8(value).map_err(|_| "record field is not ASCII-compatible")
}

// Build the literal relative symlink target, including its defensive `/.` suffix.
fn canonical_target(checkout_name: &str, nonce: &str) -> Option<String> {
    valid_nonce(nonce).then(|| format!(".{checkout_name}.install.lock.owner.{nonce}/."))
}

// Match locale-C awk field normalization for BSD `ps -o lstart=` output.
fn normalize_ps_lstart(input: &[u8]) -> Vec<u8> {
    input
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join(&b' ')
}

// Parse the shared bounded decimal timeout grammar before host arithmetic.
fn parse_timeout(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let normalized = value.trim_start_matches('0');
    let normalized = if normalized.is_empty() {
        "0"
    } else {
        normalized
    };
    (normalized.len() <= 9)
        .then(|| normalized.parse::<u64>().ok())
        .flatten()
}

// Convert caller-configurable roots into the exact physical spelling required
// by the cross-process protocol. Shdeps historically accepts relative roots and
// benign `.`/`..` spellings, while the wire contract intentionally rejects
// those spellings so independent writers cannot lock different siblings. The
// coordinator resolves that compatibility boundary once before protocol code
// sees the path.
fn normalize_requested_checkout(requested_checkout: &Path) -> io::Result<PathBuf> {
    let absolute = if requested_checkout.is_absolute() {
        requested_checkout.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested_checkout)
    };
    if absolute
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r'))
    {
        return Err(invalid_input(
            "managed checkout path must not contain line breaks",
        ));
    }
    let raw = absolute.as_os_str().as_bytes();
    let end = raw
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(0, |index| index + 1);
    let terminal = &raw[..end];
    if terminal.ends_with(b"/.") || terminal.ends_with(b"/..") {
        return Err(invalid_input(
            "managed checkout path must not end in `.` or `..`",
        ));
    }
    let name = checkout_basename(&absolute)?;
    let requested_parent = absolute
        .parent()
        .ok_or_else(|| invalid_input("managed checkout path has no parent"))?;

    // Match the installer's mkdir-p then physical-pwd model. This preserves
    // Unix symlink/`..` semantics while converting Shdeps' historically
    // accepted relative and redundant spellings into the strict v1 form.
    fs::create_dir_all(requested_parent)?;
    let parent = fs::canonicalize(requested_parent)?;
    let normalized = parent.join(name);
    if !normalized_absolute_path(&normalized) || checkout_basename(&normalized).is_err() {
        return Err(invalid_input(format!(
            "managed checkout path could not be normalized: {}",
            requested_checkout.display()
        )));
    }
    Ok(normalized)
}

// Run one mutation while holding the shared lock and report strict release errors.
pub(crate) fn with_checkout_lock<T>(
    requested_checkout: &Path,
    env: &BTreeMap<String, String>,
    operation: impl FnOnce(&Path) -> crate::Result<T>,
) -> crate::Result<T> {
    let timeout = if let Some(timeout_input) = env.get(TIMEOUT_ENV) {
        parse_timeout(timeout_input).ok_or_else(|| {
            invalid_input(format!(
                "{TIMEOUT_ENV} must be a nonnegative integer of at most 9 decimal digits: {timeout_input}"
            ))
        })?
    } else {
        DEFAULT_TIMEOUT_SECS
    };
    with_checkout_lock_timeout(requested_checkout, Duration::from_secs(timeout), operation)
}

// Use the live process environment for command paths without a captured runtime map.
pub(crate) fn with_checkout_lock_process_env<T>(
    requested_checkout: &Path,
    operation: impl FnOnce(&Path) -> crate::Result<T>,
) -> crate::Result<T> {
    let mut env = BTreeMap::new();
    if let Ok(value) = std::env::var(TIMEOUT_ENV) {
        env.insert(TIMEOUT_ENV.to_owned(), value);
    }
    with_checkout_lock(requested_checkout, &env, operation)
}

// Keep the deadline injectable for focused tests without adding runtime test knobs.
fn with_checkout_lock_timeout<T>(
    requested_checkout: &Path,
    timeout: Duration,
    operation: impl FnOnce(&Path) -> crate::Result<T>,
) -> crate::Result<T> {
    let normalized_checkout = normalize_requested_checkout(requested_checkout)?;
    let mut lock = CheckoutLock::acquire(&normalized_checkout, timeout)?;
    let operation_result = operation(&lock.paths.checkout);
    let release_result = lock.release();
    match (operation_result, release_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(operation_error), Err(release_error)) => Err(io::Error::other(format!(
            "checkout operation failed: {operation_error}; additionally failed to release checkout lock: {release_error}"
        ))
        .into()),
    }
}

// Construct caller-facing validation errors consistently across protocol checks.
fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

// Construct stable malformed-state failures without granting recovery authority.
fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

// Observe a path without following symlinks; only NotFound proves absence.
fn exists_no_follow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

// Use a conservative predicate in race rechecks where ambiguity means present.
fn path_exists(path: &Path) -> bool {
    exists_no_follow(path).unwrap_or(true)
}

// Require one private object to belong to this effective user with exact mode bits.
fn validate_private(path: &Path, expected_mode: u32, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let expected_type = if directory {
        file_type.is_dir() && !file_type.is_symlink()
    } else {
        file_type.is_file() && !file_type.is_symlink()
    };
    if !expected_type
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != expected_mode
    {
        return Err(invalid_data(format!(
            "checkout lock object has unsafe type, owner, or mode: {}",
            path.display()
        )));
    }
    Ok(())
}

// Require an exact fixed child set before moving or removing private directories.
fn validate_children(directory: &Path, expected: &[&str]) -> io::Result<()> {
    validate_private(directory, 0o700, true)?;
    let mut actual = fs::read_dir(directory)?
        .map(|entry| {
            entry.and_then(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid_data("checkout lock child name is not UTF-8"))
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    actual.sort();
    let mut wanted = expected
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    wanted.sort();
    if actual != wanted {
        return Err(invalid_data(format!(
            "checkout lock directory has unexpected contents: {}",
            directory.display()
        )));
    }
    Ok(())
}

// Read the exact kernel node-name bytes produced by the protocol's `uname -n`.
fn hostname_hex() -> io::Result<String> {
    let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
    let status = unsafe { libc::uname(uts.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    let uts = unsafe { uts.assume_init() };
    let bytes = uts
        .nodename
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid_data("host name exceeds checkout lock bound"))?;
    if length == 0 {
        return Err(invalid_data("host name is empty"));
    }
    Ok(encode_hex(&bytes[..length]))
}

// Derive a process start token from Linux procfs or the portable BSD ps fallback.
fn process_identity(pid: libc::pid_t) -> io::Result<(String, String, u8)> {
    let proc_stat = PathBuf::from(format!("/proc/{pid}/stat"));
    if let Ok(stat) = fs::read_to_string(&proc_stat) {
        let (_, rest) = stat
            .rsplit_once(") ")
            .ok_or_else(|| invalid_data("process stat record is malformed"))?;
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 20
            || fields[0].len() != 1
            || !fields[19].bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid_data("process stat record is incomplete"));
        }
        return Ok((
            PROC_STAT_KIND_HEX.to_owned(),
            encode_hex(fields[19].as_bytes()),
            fields[0].as_bytes()[0],
        ));
    }

    let lstart = command_output("ps", &["-o", "lstart=", "-p", &pid.to_string()])?;
    let lstart = normalize_ps_lstart(&lstart);
    if lstart.is_empty() {
        return Err(invalid_data("ps returned no process start time"));
    }
    let status = command_output("ps", &["-o", "stat=", "-p", &pid.to_string()])?;
    let state = status
        .split(|byte| byte.is_ascii_whitespace())
        .find(|field| !field.is_empty())
        .and_then(|field| field.first())
        .copied()
        .ok_or_else(|| invalid_data("ps returned no process state"))?;
    Ok((PS_LSTART_KIND_HEX.to_owned(), encode_hex(&lstart), state))
}

// Run one locale-C probe with all standard streams bounded and noninteractive.
fn command_output(program: &str, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} exited with {}",
            output.status
        )));
    }
    Ok(output.stdout)
}

// Capture the complete identity written into owner and claimant records.
fn current_identity() -> io::Result<ProcessIdentity> {
    let pid = libc::pid_t::try_from(std::process::id())
        .map_err(|_| invalid_data("current pid does not fit the host pid type"))?;
    let (start_kind_hex, start_token_hex, state) = process_identity(pid)?;
    Ok(ProcessIdentity {
        host_hex: hostname_hex()?,
        start_kind_hex,
        start_token_hex,
        state,
    })
}

// Classify one record conservatively: only positive local evidence permits recovery.
fn record_liveness(record: &Record) -> Liveness {
    let Ok(host_hex) = hostname_hex() else {
        return Liveness::Unknown;
    };
    if host_hex != record.host_hex {
        return Liveness::Unknown;
    }
    let Ok(pid) = record.pid.parse::<libc::pid_t>() else {
        // A positive decimal value larger than this host's pid_t can never
        // identify a live process here. Treat it like any other demonstrably
        // missing PID, matching the Actions implementation's kill/proc result.
        return Liveness::Dead;
    };
    if let Ok((kind, token, state)) = process_identity(pid) {
        if state == b'Z' {
            return Liveness::Dead;
        }
        if kind != record.start_kind_hex {
            return Liveness::Unknown;
        }
        return if token == record.start_token_hex {
            Liveness::Live
        } else {
            Liveness::Dead
        };
    }

    let signal_status = unsafe { libc::kill(pid, 0) };
    if signal_status == 0 {
        return Liveness::Unknown;
    }
    let signal_error = io::Error::last_os_error();
    if signal_error.raw_os_error() == Some(libc::EPERM) {
        return Liveness::Unknown;
    }
    if signal_error.raw_os_error() != Some(libc::ESRCH) {
        return Liveness::Unknown;
    }
    if fs::File::open("/proc/self/stat").is_ok() {
        return if path_exists(Path::new(&format!("/proc/{pid}"))) {
            Liveness::Unknown
        } else {
            Liveness::Dead
        };
    }

    let output = Command::new("ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => Liveness::Unknown,
        Ok(output)
            if output.status.code() == Some(1)
                && output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) =>
        {
            Liveness::Dead
        }
        _ => Liveness::Unknown,
    }
}

// Generate one unpredictable generation identifier without adding a RNG dependency.
fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(encode_hex(&bytes))
}

// Publish one fixed wire record with private permissions and complete framing.
fn write_record(
    path: &Path,
    role: Role,
    record_nonce: &str,
    owner_nonce: Option<&str>,
    pid: &str,
    identity: &ProcessIdentity,
    checkout: &Path,
) -> io::Result<()> {
    if !valid_nonce(record_nonce)
        || !valid_pid(pid)
        || matches!(role, Role::Owner) && owner_nonce.is_some()
        || matches!(role, Role::Claim) && !owner_nonce.is_some_and(valid_nonce)
    {
        return Err(invalid_input(
            "refusing to write an invalid checkout lock record",
        ));
    }
    let content = format!(
        "{HEADER}\nrole={}\nnonce={record_nonce}\nowner_nonce={}\npid={pid}\nhost_hex={}\nstart_kind_hex={}\nstart_token_hex={}\ncheckout_hex={}\n",
        role.wire_hex(),
        owner_nonce.unwrap_or_default(),
        identity.host_hex,
        identity.start_kind_hex,
        identity.start_token_hex,
        checkout_hex(checkout)
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

// Load and bind a private record to its expected path generation and checkout.
fn load_record(
    path: &Path,
    role: Role,
    expected_nonce: &str,
    expected_owner_nonce: Option<&str>,
    checkout: &Path,
) -> io::Result<Record> {
    validate_private(path, 0o600, false)?;
    let metadata = fs::metadata(path)?;
    if metadata.len() > 16_384 {
        return Err(invalid_data("checkout lock record exceeds its wire bound"));
    }
    let bytes = fs::read(path)?;
    parse_record(&bytes, role, expected_nonce, expected_owner_nonce, checkout).map_err(invalid_data)
}

// Create one private preparation directory whose name is never part of arbitration.
fn prepare_directory(paths: &Paths, kind: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let candidate = paths.parent.join(format!(
            ".{}.install.lock.{kind}.prepare.{}",
            paths.name,
            nonce()?
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => {
                if let Err(error) =
                    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))
                {
                    let _ = fs::remove_dir(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique checkout lock preparation directory",
    ))
}

// Prepare a complete owner privately before exposing its final generation name.
fn prepare_owner(paths: &Paths) -> io::Result<Owner> {
    let owner_nonce = nonce()?;
    let prepare = prepare_directory(paths, "owner")?;
    let record = prepare.join("owner-v1");
    let dir = paths.owner_dir(&owner_nonce);
    let mut published_private = false;
    let prepare_result = (|| -> io::Result<()> {
        let identity = current_identity()?;
        let pid = std::process::id().to_string();
        write_record(
            &record,
            Role::Owner,
            &owner_nonce,
            None,
            &pid,
            &identity,
            &paths.checkout,
        )?;
        if exists_no_follow(&dir)? {
            return Err(invalid_data(
                "checkout lock owner nonce unexpectedly already exists",
            ));
        }
        fs::rename(&prepare, &dir)?;
        published_private = true;
        validate_children(&dir, &["owner-v1"])?;
        load_record(
            &dir.join("owner-v1"),
            Role::Owner,
            &owner_nonce,
            None,
            &paths.checkout,
        )?;
        Ok(())
    })();
    if let Err(error) = prepare_result {
        cleanup_known_record_dir(&prepare, "owner-v1");
        if published_private {
            cleanup_known_record_dir(&dir, "owner-v1");
        }
        return Err(error);
    }
    Ok(Owner {
        target: paths.owner_target(&owner_nonce),
        nonce: owner_nonce,
        dir,
    })
}

// Remove only a caller-owned generation that never reached the canonical path.
fn discard_unpublished_owner(owner: &Owner) -> io::Result<()> {
    if !exists_no_follow(&owner.dir)? {
        return Ok(());
    }
    validate_children(&owner.dir, &["owner-v1"])?;
    fs::remove_file(owner.dir.join("owner-v1"))?;
    fs::remove_dir(&owner.dir)
}

// Prepare a claimant privately before it attempts ownership transfer.
fn prepare_claim(paths: &Paths, owner_nonce: &str) -> io::Result<Claim> {
    let claimant_nonce = nonce()?;
    let prepare = prepare_directory(paths, "claim")?;
    let record = prepare.join("claim-v1");
    let dir = paths.claim_dir(owner_nonce, &claimant_nonce);
    let mut published_private = false;
    let prepare_result = (|| -> io::Result<()> {
        let identity = current_identity()?;
        let pid = std::process::id().to_string();
        write_record(
            &record,
            Role::Claim,
            &claimant_nonce,
            Some(owner_nonce),
            &pid,
            &identity,
            &paths.checkout,
        )?;
        if exists_no_follow(&dir)? {
            return Err(invalid_data(
                "checkout lock claim nonce unexpectedly already exists",
            ));
        }
        fs::rename(&prepare, &dir)?;
        published_private = true;
        validate_children(&dir, &["claim-v1"])?;
        load_record(
            &dir.join("claim-v1"),
            Role::Claim,
            &claimant_nonce,
            Some(owner_nonce),
            &paths.checkout,
        )?;
        Ok(())
    })();
    if let Err(error) = prepare_result {
        cleanup_known_record_dir(&prepare, "claim-v1");
        if published_private {
            cleanup_known_record_dir(&dir, "claim-v1");
        }
        return Err(error);
    }
    Ok(Claim {
        nonce: claimant_nonce,
        dir,
    })
}

// Remove only a known preparation/final record directory created by this process.
fn cleanup_known_record_dir(directory: &Path, record_name: &str) {
    let _ = fs::remove_file(directory.join(record_name));
    let _ = fs::remove_dir(directory);
}

// Decode the owner and claimant nonces bound into one claim basename.
fn claim_name(paths: &Paths, claim_dir: &Path) -> io::Result<(String, String)> {
    let base = claim_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("checkout lock claim name is not UTF-8"))?;
    let prefix = format!(".{}.install.lock.claim.", paths.name);
    let rest = base
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid_data("checkout lock claim has the wrong prefix"))?;
    let (owner_nonce, claimant_nonce) = rest
        .split_once('.')
        .ok_or_else(|| invalid_data("checkout lock claim is missing a nonce"))?;
    if !valid_nonce(owner_nonce) || !valid_nonce(claimant_nonce) || claimant_nonce.contains('.') {
        return Err(invalid_data("checkout lock claim has an invalid nonce"));
    }
    Ok((owner_nonce.to_owned(), claimant_nonce.to_owned()))
}

// Load one claimant only after its name, mode, contents, and owner binding agree.
fn load_claim(paths: &Paths, claim_dir: &Path, owner_nonce: &str) -> io::Result<Record> {
    let (named_owner, claimant_nonce) = claim_name(paths, claim_dir)?;
    if named_owner != owner_nonce {
        return Err(invalid_data("checkout lock claim owner binding differs"));
    }
    validate_private(claim_dir, 0o700, true)?;
    load_record(
        &claim_dir.join("claim-v1"),
        Role::Claim,
        &claimant_nonce,
        Some(owner_nonce),
        &paths.checkout,
    )
}

// Remove an empty, validated claimant without touching any owner generation.
fn cleanup_empty_claim(paths: &Paths, claim: &Claim, owner_nonce: &str) -> io::Result<()> {
    validate_children(&claim.dir, &["claim-v1"])?;
    load_record(
        &claim.dir.join("claim-v1"),
        Role::Claim,
        &claim.nonce,
        Some(owner_nonce),
        &paths.checkout,
    )?;
    fs::remove_file(claim.dir.join("claim-v1"))?;
    fs::remove_dir(&claim.dir)
}

// Retire a detached owner before leaf cleanup so fresh generations may proceed.
fn retire_and_cleanup_claim(paths: &Paths, claim_dir: &Path, owner_nonce: &str) -> io::Result<()> {
    let owner_dir = claim_dir.join("owner");
    let retired_dir = claim_dir.join("retired");
    validate_children(claim_dir, &["claim-v1", "owner"])?;
    load_claim(paths, claim_dir, owner_nonce)?;
    validate_children(&owner_dir, &["canonical", "owner-v1"])?;
    load_record(
        &owner_dir.join("owner-v1"),
        Role::Owner,
        owner_nonce,
        None,
        &paths.checkout,
    )?;
    let expected_target = paths.owner_target(owner_nonce);
    if !symlink_target_equals(&owner_dir.join("canonical"), &expected_target) {
        return Err(invalid_data("checkout lock tombstone target differs"));
    }
    if exists_no_follow(&retired_dir)? {
        return Err(invalid_data(
            "checkout lock retired slot is already occupied",
        ));
    }

    fs::rename(&owner_dir, &retired_dir)?;
    // Detachment and retirement have committed. Never inspect the canonical
    // path below this boundary; another writer may already own it.
    validate_children(&retired_dir, &["canonical", "owner-v1"])?;
    fs::remove_file(retired_dir.join("canonical"))?;
    fs::remove_file(retired_dir.join("owner-v1"))?;
    fs::remove_dir(&retired_dir)?;
    fs::remove_file(claim_dir.join("claim-v1"))?;
    fs::remove_dir(claim_dir)?;
    Ok(())
}

// Detach a claimed generation only after revalidating both records and target.
fn detach_claimed_owner(
    paths: &Paths,
    claim_dir: &Path,
    owner_nonce: &str,
    owner_target: &str,
) -> io::Result<()> {
    let owner_dir = claim_dir.join("owner");
    validate_children(claim_dir, &["claim-v1", "owner"])?;
    load_claim(paths, claim_dir, owner_nonce)?;
    validate_children(&owner_dir, &["owner-v1"])?;
    load_record(
        &owner_dir.join("owner-v1"),
        Role::Owner,
        owner_nonce,
        None,
        &paths.checkout,
    )?;
    if !symlink_target_equals(&paths.canonical, owner_target) {
        return Err(invalid_data("checkout lock canonical generation changed"));
    }
    let tombstone = owner_dir.join("canonical");
    if exists_no_follow(&tombstone)? {
        return Err(invalid_data("checkout lock tombstone already exists"));
    }
    fs::rename(&paths.canonical, &tombstone)?;
    if !symlink_target_equals(&tombstone, owner_target) {
        return Err(invalid_data("checkout lock tombstone publication failed"));
    }
    // The canonical detachment is now committed. The retirement helper is
    // deliberately forbidden from consulting that path again.
    retire_and_cleanup_claim(paths, claim_dir, owner_nonce)
}

// Claim one complete owner directory; that rename is the ownership linearization point.
fn claim_and_detach(paths: &Paths, owner: &Owner) -> io::Result<()> {
    let claim = prepare_claim(paths, &owner.nonce)?;
    let nested_owner = claim.dir.join("owner");
    if exists_no_follow(&nested_owner)? {
        cleanup_empty_claim(paths, &claim, &owner.nonce)?;
        return Err(invalid_data("checkout lock claim owner slot is occupied"));
    }
    if let Err(error) = fs::rename(&owner.dir, &nested_owner) {
        let _ = cleanup_empty_claim(paths, &claim, &owner.nonce);
        return Err(error);
    }
    detach_claimed_owner(paths, &claim.dir, &owner.nonce, &owner.target)
}

// Strictly release only the exact generation recorded by this guard.
fn release_owner(paths: &Paths, owner: &Owner) -> io::Result<()> {
    if !symlink_target_equals(&paths.canonical, &owner.target) {
        return Err(invalid_data(
            "checkout lock release no longer owns the canonical path",
        ));
    }
    validate_children(&owner.dir, &["owner-v1"])?;
    load_record(
        &owner.dir.join("owner-v1"),
        Role::Owner,
        &owner.nonce,
        None,
        &paths.checkout,
    )?;
    claim_and_detach(paths, owner)
}

// Compare a symlink's literal bytes so the v1 `/.` suffix cannot normalize away.
fn symlink_target_equals(path: &Path, expected: &str) -> bool {
    fs::read_link(path)
        .ok()
        .is_some_and(|target| target.as_os_str().as_bytes() == expected.as_bytes())
}

// Parse only the one exact relative owner target accepted by protocol v1.
fn owner_nonce_from_target(paths: &Paths, target: &Path) -> Option<String> {
    let target = target.as_os_str().as_bytes();
    let prefix = format!(".{}.install.lock.owner.", paths.name);
    let prefix = prefix.as_bytes();
    let suffix = b"/.";
    if !target.starts_with(prefix) || !target.ends_with(suffix) {
        return None;
    }
    let nonce = std::str::from_utf8(&target[prefix.len()..target.len() - suffix.len()]).ok()?;
    valid_nonce(nonce).then(|| nonce.to_owned())
}

// Recheck whether the canonical path still names one generation during a race.
fn canonical_matches(paths: &Paths, target: &str) -> bool {
    symlink_target_equals(&paths.canonical, target)
}

// Discover exactly one owner-bearing claim for a dangling canonical generation.
fn find_claimed_owner(paths: &Paths, owner_nonce: &str, owner_target: &str) -> Classified {
    let prefix = paths.claim_prefix(owner_nonce);
    let mut claims = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.parent) else {
        return Classified::Retry;
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return Classified::Retry,
        };
        let file_name = entry.file_name();
        if !file_name.as_bytes().starts_with(prefix.as_bytes()) {
            continue;
        }
        let claim_dir = entry.path();
        let nested_owner = claim_dir.join("owner");
        if !path_exists(&nested_owner) {
            continue;
        }
        if file_name.to_str().is_none() {
            return Classified::Malformed(
                "owner-bearing checkout lock claim has a non-UTF-8 name".to_owned(),
            );
        }
        let validation = (|| -> io::Result<(Record, Record)> {
            validate_children(&claim_dir, &["claim-v1", "owner"])?;
            let claim_record = load_claim(paths, &claim_dir, owner_nonce)?;
            validate_children(&nested_owner, &["owner-v1"])?;
            let owner_record = load_record(
                &nested_owner.join("owner-v1"),
                Role::Owner,
                owner_nonce,
                None,
                &paths.checkout,
            )?;
            Ok((claim_record, owner_record))
        })();
        let Ok((claim_record, _owner_record)) = validation else {
            if !canonical_matches(paths, owner_target)
                || !path_exists(&claim_dir)
                || !path_exists(&nested_owner)
            {
                return Classified::Retry;
            }
            return Classified::Malformed(
                "owner-bearing checkout lock claim is malformed".to_owned(),
            );
        };
        claims.push((
            claim_dir,
            claim_record.pid.clone(),
            record_liveness(&claim_record),
        ));
    }

    if !canonical_matches(paths, owner_target) {
        return Classified::Retry;
    }
    match claims.as_slice() {
        [] => Classified::Retry,
        [(claim_dir, pid, liveness)] => Classified::Claim {
            liveness: *liveness,
            pid: pid.clone(),
            owner_nonce: owner_nonce.to_owned(),
            owner_target: owner_target.to_owned(),
            claim_dir: claim_dir.clone(),
        },
        _ => Classified::Malformed("multiple owner-bearing checkout lock claims exist".to_owned()),
    }
}

// Classify the canonical object without changing any protocol path.
fn classify(paths: &Paths) -> Classified {
    let metadata = match fs::symlink_metadata(&paths.canonical) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Classified::Missing,
        Err(error) => return Classified::Malformed(error.to_string()),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return Classified::Legacy;
    }
    if !metadata.file_type().is_symlink() {
        return Classified::Malformed("canonical lock is not a symlink".to_owned());
    }
    let Ok(target) = fs::read_link(&paths.canonical) else {
        return Classified::Retry;
    };
    let Some(owner_nonce) = owner_nonce_from_target(paths, &target) else {
        return Classified::Malformed("canonical lock target is hostile".to_owned());
    };
    let owner_target = paths.owner_target(&owner_nonce);
    let owner_dir = paths.owner_dir(&owner_nonce);
    if !path_exists(&owner_dir) {
        return find_claimed_owner(paths, &owner_nonce, &owner_target);
    }
    let validation = (|| -> io::Result<Record> {
        validate_children(&owner_dir, &["owner-v1"])?;
        load_record(
            &owner_dir.join("owner-v1"),
            Role::Owner,
            &owner_nonce,
            None,
            &paths.checkout,
        )
    })();
    let Ok(record) = validation else {
        if !path_exists(&owner_dir) || !canonical_matches(paths, &owner_target) {
            return Classified::Retry;
        }
        return Classified::Malformed("checkout lock owner is malformed".to_owned());
    };
    Classified::Owner {
        liveness: record_liveness(&record),
        pid: record.pid,
        owner: Owner {
            nonce: owner_nonce,
            target: owner_target,
            dir: owner_dir,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    Complete,
    Retry,
}

// Clean only detached dead claims; active or retired claims never block acquisition.
fn cleanup_detached_claims(paths: &Paths) -> io::Result<Cleanup> {
    if path_exists(&paths.canonical) {
        return Ok(Cleanup::Retry);
    }
    let prefix = format!(".{}.install.lock.claim.", paths.name);
    let entries = fs::read_dir(&paths.parent)?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) if path_exists(&paths.canonical) => return Ok(Cleanup::Retry),
            Err(error) => return Err(error),
        };
        let file_name = entry.file_name();
        if !file_name.as_bytes().starts_with(prefix.as_bytes()) {
            continue;
        }
        let claim_dir = entry.path();
        let nested_owner = claim_dir.join("owner");
        if !path_exists(&nested_owner) {
            continue;
        }
        if file_name.to_str().is_none() {
            return Err(invalid_data(
                "owner-bearing checkout lock claim has a non-UTF-8 name",
            ));
        }
        let validation = (|| -> io::Result<(String, Record)> {
            validate_children(&claim_dir, &["claim-v1", "owner"])?;
            let (owner_nonce, _) = claim_name(paths, &claim_dir)?;
            let claim_record = load_claim(paths, &claim_dir, &owner_nonce)?;
            validate_children(&nested_owner, &["canonical", "owner-v1"])?;
            load_record(
                &nested_owner.join("owner-v1"),
                Role::Owner,
                &owner_nonce,
                None,
                &paths.checkout,
            )?;
            if !symlink_target_equals(
                &nested_owner.join("canonical"),
                &paths.owner_target(&owner_nonce),
            ) {
                return Err(invalid_data("detached checkout lock tombstone differs"));
            }
            Ok((owner_nonce, claim_record))
        })();
        let Ok((owner_nonce, claim_record)) = validation else {
            if path_exists(&paths.canonical)
                || !path_exists(&claim_dir)
                || !path_exists(&nested_owner)
            {
                return Ok(Cleanup::Retry);
            }
            return Err(invalid_data(format!(
                "detached checkout lock claim is malformed: {}",
                claim_dir.display()
            )));
        };
        if record_liveness(&claim_record) == Liveness::Dead {
            if let Err(error) = retire_and_cleanup_claim(paths, &claim_dir, &owner_nonce) {
                if path_exists(&paths.canonical)
                    || !path_exists(&claim_dir)
                    || !path_exists(&nested_owner)
                {
                    return Ok(Cleanup::Retry);
                }
                return Err(error);
            }
        }
    }
    if path_exists(&paths.canonical) {
        Ok(Cleanup::Retry)
    } else {
        Ok(Cleanup::Complete)
    }
}

// Reclaim one live-path owner only after a second positive dead-owner probe.
fn recover_dead_owner(paths: &Paths, owner: &Owner) -> io::Result<()> {
    validate_children(&owner.dir, &["owner-v1"])?;
    let record = load_record(
        &owner.dir.join("owner-v1"),
        Role::Owner,
        &owner.nonce,
        None,
        &paths.checkout,
    )?;
    if record_liveness(&record) != Liveness::Dead {
        return Err(invalid_data("checkout lock owner is no longer proven dead"));
    }
    claim_and_detach(paths, owner)
}

// Take over the sole owner-bearing claim only after proving its claimant dead again.
fn recover_dead_claim(
    paths: &Paths,
    old_claim: &Path,
    owner_nonce: &str,
    owner_target: &str,
) -> io::Result<()> {
    validate_children(old_claim, &["claim-v1", "owner"])?;
    let old_record = load_claim(paths, old_claim, owner_nonce)?;
    if record_liveness(&old_record) != Liveness::Dead {
        return Err(invalid_data(
            "checkout lock claimant is no longer proven dead",
        ));
    }
    let old_owner = old_claim.join("owner");
    validate_children(&old_owner, &["owner-v1"])?;
    load_record(
        &old_owner.join("owner-v1"),
        Role::Owner,
        owner_nonce,
        None,
        &paths.checkout,
    )?;
    if !canonical_matches(paths, owner_target) {
        return Err(invalid_data("checkout lock canonical generation changed"));
    }

    let new_claim = prepare_claim(paths, owner_nonce)?;
    let new_owner = new_claim.dir.join("owner");
    if let Err(error) = fs::rename(&old_owner, &new_owner) {
        let _ = cleanup_empty_claim(paths, &new_claim, owner_nonce);
        return Err(error);
    }
    let (old_owner_nonce, old_claimant_nonce) = claim_name(paths, old_claim)?;
    let old_claim_ref = Claim {
        nonce: old_claimant_nonce,
        dir: old_claim.to_path_buf(),
    };
    // Ownership has already moved into the new claim. The old claim no longer
    // participates in arbitration, so cleanup failure must not strand the live
    // claimant behind its own record until timeout.
    let _ = cleanup_empty_claim(paths, &old_claim_ref, &old_owner_nonce);
    detach_claimed_owner(paths, &new_claim.dir, owner_nonce, owner_target)
}

// Format a terminal wait error using only holder data from validated records.
fn timeout_error(paths: &Paths, timeout: Duration, state: &Classified) -> io::Error {
    let mut message = format!(
        "timed out after {}s waiting for managed checkout lock: {}",
        timeout.as_secs(),
        paths.canonical.display()
    );
    match state {
        Classified::Owner { pid, .. } | Classified::Claim { pid, .. } if valid_pid(pid) => {
            message.push_str(&format!(" (holder pid {pid})"));
        }
        Classified::Legacy => message.push_str(
            "; if no install is running, remove that exact empty directory with rmdir and retry",
        ),
        _ => {}
    }
    io::Error::new(io::ErrorKind::TimedOut, message)
}

// Acquire through bounded classify/recover/publish retries without broad cleanup.
fn acquire(paths: &Paths, timeout: Duration) -> io::Result<CheckoutLock> {
    let started = Instant::now();
    loop {
        let state = classify(paths);
        match &state {
            Classified::Missing => match cleanup_detached_claims(paths)? {
                Cleanup::Complete => {
                    let owner = prepare_owner(paths)?;
                    match symlink(&owner.target, &paths.canonical) {
                        Ok(()) => {
                            if !canonical_matches(paths, &owner.target) {
                                return Err(invalid_data(
                                    "checkout lock publication could not be verified",
                                ));
                            }
                            load_record(
                                &owner.dir.join("owner-v1"),
                                Role::Owner,
                                &owner.nonce,
                                None,
                                &paths.checkout,
                            )?;
                            return Ok(CheckoutLock {
                                paths: paths.clone(),
                                owner: Some(owner),
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            discard_unpublished_owner(&owner)?;
                        }
                        Err(error) => {
                            let cleanup = discard_unpublished_owner(&owner);
                            return cleanup.and(Err(error));
                        }
                    }
                }
                Cleanup::Retry => {}
            },
            Classified::Owner {
                liveness: Liveness::Dead,
                owner,
                ..
            } => {
                if recover_dead_owner(paths, owner).is_ok() {
                    continue;
                }
            }
            Classified::Claim {
                liveness: Liveness::Dead,
                owner_nonce,
                owner_target,
                claim_dir,
                ..
            } => {
                if recover_dead_claim(paths, claim_dir, owner_nonce, owner_target).is_ok() {
                    continue;
                }
            }
            Classified::Malformed(detail) => {
                return Err(invalid_data(format!(
                    "managed checkout lock is malformed; refusing to change it: {} ({detail})",
                    paths.canonical.display()
                )));
            }
            Classified::Legacy
            | Classified::Owner { .. }
            | Classified::Claim { .. }
            | Classified::Retry => {}
        }

        if started.elapsed() >= timeout {
            let final_state = classify(paths);
            if let Classified::Malformed(detail) = &final_state {
                return Err(invalid_data(format!(
                    "managed checkout lock is malformed; refusing to change it: {} ({detail})",
                    paths.canonical.display()
                )));
            }
            return Err(timeout_error(paths, timeout, &final_state));
        }
        thread::sleep(std::cmp::min(
            WAIT_POLL,
            timeout.saturating_sub(started.elapsed()),
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        CheckoutLock, Classified, Cleanup, Liveness, Paths, ProcessIdentity, Record, Role,
        canonical_target, classify, cleanup_detached_claims, cleanup_empty_claim, current_identity,
        discard_unpublished_owner, hostname_hex, normalize_ps_lstart, parse_record, parse_timeout,
        path_exists, prepare_claim, prepare_owner, process_identity, record_liveness,
        retire_and_cleanup_claim, with_checkout_lock, with_checkout_lock_timeout, write_record,
    };

    const OWNER_RECORD: &[u8] =
        include_bytes!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-owner-record.txt");
    const CLAIM_RECORD: &[u8] =
        include_bytes!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-claim-record.txt");
    const RECORD_VECTORS: &str =
        include_str!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-records.tsv");
    const WIRE_VECTORS: &str =
        include_str!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-wire.tsv");
    const STATE_VECTORS: &str =
        include_str!("../tests/fixtures/checkout-lock-v1/checkout-lock-v1-states.tsv");

    // Convert the fixture's lowercase hex bytes without accepting malformed input.
    fn decode_hex(value: &str) -> Vec<u8> {
        assert_eq!(value.len() % 2, 0);
        (0..value.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&value[offset..offset + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_authoritative_owner_and_claim_records_verbatim() {
        assert!(
            parse_record(
                OWNER_RECORD,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_ok()
        );
        assert!(
            parse_record(
                CLAIM_RECORD,
                Role::Claim,
                "fedcba9876543210fedcba9876543210",
                Some("0123456789abcdef0123456789abcdef"),
                Path::new("/tmp/tool"),
            )
            .is_ok()
        );
    }

    #[test]
    fn enforces_every_authoritative_record_vector() {
        for line in RECORD_VECTORS.lines().filter(|line| !line.starts_with('#')) {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 10, "malformed record vector: {line}");
            let role = match fields[1] {
                "owner" => Role::Owner,
                "claim" => Role::Claim,
                _ => Role::Owner,
            };
            let owner_nonce = (fields[3] != "-").then_some(fields[3]);
            let role_hex = match fields[1] {
                "owner" => "6f776e6572",
                "claim" => "636c61696d",
                _ => "6f74686572",
            };
            let record = format!(
                "cgraf78 checkout mutation lock v1\nrole={role_hex}\nnonce={}\nowner_nonce={}\npid={}\nhost_hex={}\nstart_kind_hex={}\nstart_token_hex={}\ncheckout_hex={}\n",
                fields[2],
                owner_nonce.unwrap_or_default(),
                fields[4],
                fields[5],
                fields[6],
                fields[7],
                fields[8]
            );
            let parsed = parse_record(
                record.as_bytes(),
                role,
                fields[2],
                owner_nonce,
                Path::new("/tmp/tool"),
            );

            assert_eq!(
                parsed.is_ok(),
                fields[9] == "valid",
                "record vector disagreed: {}",
                fields[0]
            );
        }
    }

    #[test]
    fn rejects_raw_framing_that_shell_strings_cannot_represent() {
        let mut nul = OWNER_RECORD.to_vec();
        nul.insert(nul.len() - 1, 0);
        assert!(
            parse_record(
                &nul,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_err()
        );

        let without_final_newline = &OWNER_RECORD[..OWNER_RECORD.len() - 1];
        assert!(
            parse_record(
                without_final_newline,
                Role::Owner,
                "0123456789abcdef0123456789abcdef",
                None,
                Path::new("/tmp/tool"),
            )
            .is_err()
        );
    }

    #[test]
    fn enforces_authoritative_wire_transformations() {
        for line in WIRE_VECTORS.lines().filter(|line| !line.starts_with('#')) {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 3, "malformed wire vector: {line}");
            match fields[0] {
                "canonical-target" => {
                    let (name, nonce) = fields[1].split_once('|').unwrap();
                    assert_eq!(canonical_target(name, nonce).as_deref(), Some(fields[2]));
                }
                "ps-lstart-normalize" => {
                    assert_eq!(
                        normalize_ps_lstart(&decode_hex(fields[1])),
                        decode_hex(fields[2])
                    );
                }
                other => panic!("unknown wire vector {other}"),
            }
        }
    }

    #[test]
    fn timeout_grammar_normalizes_decimal_without_octal_or_overflow() {
        assert_eq!(parse_timeout("0"), Some(0));
        assert_eq!(parse_timeout("0008"), Some(8));
        assert_eq!(parse_timeout("999999999"), Some(999_999_999));
        assert_eq!(parse_timeout(""), None);
        assert_eq!(parse_timeout(" 8"), None);
        assert_eq!(parse_timeout("1000000000"), None);
    }

    // Allocate one isolated checkout parent for real filesystem arbitration tests.
    fn checkout(name: &str) -> PathBuf {
        crate::test_support::temp_dir(&format!("shdeps-checkout-lock-{name}")).join("tool")
    }

    // Race two zero-timeout writers while holding the winner until the loser observes it.
    fn race_two_acquirers(requested: &Path) -> usize {
        let start = Arc::new(Barrier::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let loser_finished = Arc::new(AtomicBool::new(false));
        let overlap = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let requested = requested.to_path_buf();
            let start = Arc::clone(&start);
            let active = Arc::clone(&active);
            let loser_finished = Arc::clone(&loser_finished);
            let overlap = Arc::clone(&overlap);
            workers.push(thread::spawn(move || {
                start.wait();
                let result = with_checkout_lock_timeout(&requested, Duration::ZERO, |_| {
                    if active.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlap.store(true, Ordering::SeqCst);
                    }
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !loser_finished.load(Ordering::SeqCst) && !overlap.load(Ordering::SeqCst)
                    {
                        assert!(
                            Instant::now() < deadline,
                            "contending writer never resolved"
                        );
                        thread::yield_now();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                });
                match result {
                    Ok(()) => true,
                    Err(error) => {
                        // Let the winner leave its critical section before
                        // asserting diagnostics so a test failure cannot turn
                        // into a secondary artificial timeout.
                        loser_finished.store(true, Ordering::SeqCst);
                        assert!(
                            error.to_string().contains("timed out after 0s"),
                            "contender failed for the wrong reason: {error}"
                        );
                        false
                    }
                }
            }));
        }
        start.wait();
        let winners = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum();
        assert!(
            !overlap.load(Ordering::SeqCst),
            "checkout mutations overlapped"
        );
        winners
    }

    // Create one exact-mode private directory for a materialized protocol state.
    fn private_dir(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    // Materialize one valid owner record without publishing the canonical symlink.
    fn owner_state(
        paths: &Paths,
        owner_nonce: &str,
        pid: &str,
        identity: &ProcessIdentity,
    ) -> PathBuf {
        let owner_dir = paths.owner_dir(owner_nonce);
        private_dir(&owner_dir);
        write_record(
            &owner_dir.join("owner-v1"),
            Role::Owner,
            owner_nonce,
            None,
            pid,
            identity,
            &paths.checkout,
        )
        .unwrap();
        owner_dir
    }

    // Materialize one owner-bearing claim, optionally after canonical detachment.
    fn claim_state(
        paths: &Paths,
        owner_nonce: &str,
        claim_nonce: &str,
        claim_pid: &str,
        claim_identity: &ProcessIdentity,
        owner_identity: &ProcessIdentity,
        detached: bool,
    ) -> PathBuf {
        let claim_dir = paths.claim_dir(owner_nonce, claim_nonce);
        private_dir(&claim_dir);
        write_record(
            &claim_dir.join("claim-v1"),
            Role::Claim,
            claim_nonce,
            Some(owner_nonce),
            claim_pid,
            claim_identity,
            &paths.checkout,
        )
        .unwrap();
        let nested_owner = claim_dir.join("owner");
        private_dir(&nested_owner);
        write_record(
            &nested_owner.join("owner-v1"),
            Role::Owner,
            owner_nonce,
            None,
            &std::process::id().to_string(),
            owner_identity,
            &paths.checkout,
        )
        .unwrap();
        if detached {
            symlink(
                paths.owner_target(owner_nonce),
                nested_owner.join("canonical"),
            )
            .unwrap();
        }
        claim_dir
    }

    // Translate the typed classifier into the shared fixture's symbolic action.
    fn classified_action(state: &Classified) -> &'static str {
        match state {
            Classified::Missing => "acquire",
            Classified::Legacy => "wait-legacy",
            Classified::Owner {
                liveness: Liveness::Dead,
                ..
            } => "claim",
            Classified::Owner { .. } => "wait",
            Classified::Claim {
                liveness: Liveness::Dead,
                ..
            } => "reclaim",
            Classified::Claim { .. } => "wait",
            Classified::Retry => "retry",
            Classified::Malformed(_) => "fail",
        }
    }

    #[test]
    fn materializes_every_authoritative_state_vector() {
        for line in STATE_VECTORS.lines().filter(|line| !line.starts_with('#')) {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 8, "malformed state vector: {line}");
            let case = fields[0];
            let requested = checkout(&format!("state-{case}"));
            let paths = Paths::new(&requested).unwrap();
            let identity = current_identity().unwrap();
            let mut remote_identity = identity.clone();
            remote_identity.host_hex = if identity.host_hex == "00" {
                "01".to_owned()
            } else {
                "00".to_owned()
            };
            let owner_nonce = "0123456789abcdef0123456789abcdef";
            let first_claim = "11111111111111111111111111111111";
            let second_claim = "22222222222222222222222222222222";
            let contract: [&str; 6] = match case {
                "missing" => ["missing", "-", "-", "0", "-", "-"],
                "live-owner" => ["valid-symlink", "valid", "live", "0", "-", "absent"],
                "dead-owner" => ["valid-symlink", "valid", "dead", "0", "-", "absent"],
                "remote-owner" => ["valid-symlink", "valid", "unknown", "0", "-", "absent"],
                "legacy-empty" => ["legacy-directory", "-", "-", "0", "-", "-"],
                "malformed-record" => ["valid-symlink", "malformed", "-", "0", "-", "absent"],
                "hostile-target" => ["hostile-symlink", "-", "-", "0", "-", "-"],
                "dangling-no-claim" => ["dangling-symlink", "valid", "dead", "0", "-", "absent"],
                "dangling-live-claim" => {
                    ["dangling-symlink", "valid", "dead", "1", "live", "absent"]
                }
                "dangling-dead-claim" => {
                    ["dangling-symlink", "valid", "dead", "1", "dead", "absent"]
                }
                "dangling-multiple-claims" => {
                    ["dangling-symlink", "valid", "dead", "2", "dead", "absent"]
                }
                "detached-tombstone" => ["missing", "valid", "dead", "1", "dead", "present"],
                other => panic!("unknown state vector {other}"),
            };
            assert_eq!(&fields[1..7], contract, "state inputs disagreed: {case}");

            let observed = match case {
                "missing" => classified_action(&classify(&paths)),
                "live-owner" => {
                    owner_state(
                        &paths,
                        owner_nonce,
                        &std::process::id().to_string(),
                        &identity,
                    );
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "dead-owner" => {
                    owner_state(&paths, owner_nonce, "2147483647", &identity);
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "remote-owner" => {
                    owner_state(
                        &paths,
                        owner_nonce,
                        &std::process::id().to_string(),
                        &remote_identity,
                    );
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "legacy-empty" => {
                    fs::create_dir(&paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "malformed-record" => {
                    let owner_dir = paths.owner_dir(owner_nonce);
                    private_dir(&owner_dir);
                    fs::write(owner_dir.join("owner-v1"), "malformed\n").unwrap();
                    fs::set_permissions(
                        owner_dir.join("owner-v1"),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "hostile-target" => {
                    symlink("../../hostile", &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "dangling-no-claim" => {
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "dangling-live-claim" => {
                    claim_state(
                        &paths,
                        owner_nonce,
                        first_claim,
                        &std::process::id().to_string(),
                        &identity,
                        &identity,
                        false,
                    );
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "dangling-dead-claim" => {
                    claim_state(
                        &paths,
                        owner_nonce,
                        first_claim,
                        "2147483647",
                        &identity,
                        &identity,
                        false,
                    );
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "dangling-multiple-claims" => {
                    for claim_nonce in [first_claim, second_claim] {
                        claim_state(
                            &paths,
                            owner_nonce,
                            claim_nonce,
                            "2147483647",
                            &identity,
                            &identity,
                            false,
                        );
                    }
                    symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
                    classified_action(&classify(&paths))
                }
                "detached-tombstone" => {
                    let claim_dir = claim_state(
                        &paths,
                        owner_nonce,
                        first_claim,
                        "2147483647",
                        &identity,
                        &identity,
                        true,
                    );
                    assert_eq!(cleanup_detached_claims(&paths).unwrap(), Cleanup::Complete);
                    assert!(!path_exists(&claim_dir));
                    "cleanup-acquire"
                }
                other => panic!("unknown state vector {other}"),
            };

            assert_eq!(observed, fields[7], "state vector disagreed: {case}");
        }
    }

    #[test]
    fn acquisition_resolves_parent_and_releases_exact_generation() {
        let root = crate::test_support::temp_dir("shdeps-checkout-lock-normalize");
        let physical = root.join("physical");
        let alias = root.join("alias");
        fs::create_dir_all(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let requested = alias.join("tool");
        let expected = physical.join("tool");

        let observed = with_checkout_lock_timeout(&requested, Duration::ZERO, |normalized| {
            let paths = Paths::new(normalized).unwrap();
            let target = fs::read_link(&paths.canonical).unwrap();
            assert!(target.to_string_lossy().ends_with("/."));
            assert!(path_exists(&paths.parent.join(target).join("owner-v1")));
            Ok(normalized.to_path_buf())
        })
        .unwrap();

        assert_eq!(observed, expected);
        let paths = Paths::new(&requested).unwrap();
        assert!(!path_exists(&paths.canonical));
    }

    #[test]
    fn raw_non_normalized_checkout_spellings_are_rejected() {
        let root = crate::test_support::temp_dir("shdeps-checkout-lock-path-spelling");
        fs::create_dir_all(&root).unwrap();
        for raw in [
            format!("{}/./tool", root.display()),
            format!("{}//tool", root.display()),
            format!("{}/tool/", root.display()),
            format!("{}/nested/../tool", root.display()),
        ] {
            assert!(
                Paths::new(Path::new(&raw)).is_err(),
                "accepted non-normalized checkout path: {raw}"
            );
        }
    }

    #[test]
    fn coordinator_normalizes_configured_checkout_spellings_before_protocol_use() {
        let root = crate::test_support::temp_dir("shdeps-checkout-lock-coordinator-path");
        let expected = root.join("tool");
        for requested in [
            PathBuf::from(format!("{}/./tool", root.display())),
            PathBuf::from(format!("{}//tool", root.display())),
            PathBuf::from(format!("{}/tool/", root.display())),
            PathBuf::from(format!("{}/nested/../tool", root.display())),
        ] {
            let observed = with_checkout_lock_timeout(&requested, Duration::ZERO, |normalized| {
                Ok(normalized.to_path_buf())
            })
            .unwrap();
            assert_eq!(observed, expected, "request was {}", requested.display());
        }
        let physical = root.join("physical/child");
        let alias = root.join("alias");
        fs::create_dir_all(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let symlink_parent_request = alias.join("../physical-tool");
        let observed =
            with_checkout_lock_timeout(&symlink_parent_request, Duration::ZERO, |normalized| {
                Ok(normalized.to_path_buf())
            })
            .unwrap();
        assert_eq!(
            observed,
            root.join("physical/physical-tool"),
            "`..` after a symlink must use physical filesystem semantics"
        );

        let dangling = root.join("dangling");
        let dangling_target = root.join("missing-target");
        symlink(&dangling_target, &dangling).unwrap();
        assert!(
            with_checkout_lock_timeout(&dangling.join("../dangling-tool"), Duration::ZERO, |_| Ok(
                ()
            ),)
            .is_err(),
            "a dangling parent symlink must fail closed"
        );
        assert!(!dangling_target.exists());

        let invalid_parent = root.join("invalid\nparent");
        assert!(
            with_checkout_lock_timeout(&invalid_parent.join("tool"), Duration::ZERO, |_| Ok(()),)
                .is_err()
        );
        assert!(
            !invalid_parent.exists(),
            "strict validation must finish before parent creation"
        );
        let invalid_name_parent = root.join("invalid-name-parent");
        let invalid_name = OsString::from_vec(vec![b't', b'o', 0xff, b'l']);
        assert!(
            with_checkout_lock_timeout(
                &invalid_name_parent.join(invalid_name),
                Duration::ZERO,
                |_| Ok(()),
            )
            .is_err()
        );
        assert!(
            !invalid_name_parent.exists(),
            "an invalid basename must not create its parent"
        );

        let terminal_dot_parent = root.join("terminal-dot-parent");
        let terminal_dot = PathBuf::from(format!("{}/tool/.", terminal_dot_parent.display()));
        assert!(
            with_checkout_lock_timeout(&terminal_dot, Duration::ZERO, |_| Ok(())).is_err(),
            "a terminal navigation component must not select a different lock root"
        );
        assert!(
            !terminal_dot_parent.exists(),
            "terminal-dot rejection must happen before parent creation"
        );

        // Runtime roots are user-configurable and historically allowed relative
        // spellings. Use a unique path below Cargo's normal build directory so
        // this case exercises that contract without changing process-wide cwd.
        let unique = root.file_name().unwrap();
        let relative_parent = PathBuf::from("target").join(unique);
        let relative_request = relative_parent.join("tool");
        let expected_relative = std::env::current_dir().unwrap().join(&relative_request);
        let observed =
            with_checkout_lock_timeout(&relative_request, Duration::ZERO, |normalized| {
                Ok(normalized.to_path_buf())
            })
            .unwrap();
        assert_eq!(observed, expected_relative);
        fs::remove_dir_all(relative_parent).unwrap();
    }

    #[test]
    fn live_owner_times_out_without_disturbing_its_generation() {
        let requested = checkout("live");
        let mut first = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();
        let paths = Paths::new(&requested).unwrap();
        let target_before = fs::read_link(&paths.canonical).unwrap();

        let error = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            error
                .to_string()
                .contains(&format!("holder pid {}", std::process::id()))
        );
        assert_eq!(fs::read_link(&paths.canonical).unwrap(), target_before);
        first.release().unwrap();
    }

    #[test]
    fn legacy_directory_is_preserved_with_actionable_recovery_guidance() {
        let requested = checkout("legacy");
        let paths = Paths::new(&requested).unwrap();
        fs::create_dir(&paths.canonical).unwrap();

        let error = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("rmdir"));
        assert!(paths.canonical.is_dir());
        assert_eq!(fs::read_dir(&paths.canonical).unwrap().count(), 0);
    }

    #[test]
    fn malformed_canonical_object_fails_closed_without_mutation() {
        let requested = checkout("malformed");
        let paths = Paths::new(&requested).unwrap();
        fs::write(&paths.canonical, "foreign").unwrap();

        let error = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&paths.canonical).unwrap(), "foreign");
    }

    #[test]
    fn operation_failure_still_strictly_releases_the_lock() {
        let requested = checkout("operation-failure");
        let paths = Paths::new(&requested).unwrap();

        let error = with_checkout_lock_timeout(&requested, Duration::ZERO, |_| {
            Err::<(), _>(std::io::Error::other("operation failed").into())
        })
        .unwrap_err();

        assert!(error.to_string().contains("operation failed"));
        assert!(!path_exists(&paths.canonical));
    }

    #[test]
    fn public_wrapper_rejects_invalid_timeout_before_mutation() {
        let requested = checkout("invalid-timeout");
        let paths = Paths::new(&requested).unwrap();
        let env = BTreeMap::from([(
            "SHDEPS_CHECKOUT_LOCK_TIMEOUT_SECS".to_owned(),
            "1000000000".to_owned(),
        )]);

        let error = with_checkout_lock(&requested, &env, |_| Ok(())).unwrap_err();

        assert!(error.to_string().contains("at most 9 decimal digits"));
        assert!(!path_exists(&paths.canonical));
    }

    // Build a syntactically valid record around one observed process identity.
    fn process_record(pid: String, identity: &ProcessIdentity, checkout: &Path) -> Record {
        Record {
            role: Role::Owner,
            nonce: "0123456789abcdef0123456789abcdef".to_owned(),
            owner_nonce: None,
            pid,
            host_hex: identity.host_hex.clone(),
            start_kind_hex: identity.start_kind_hex.clone(),
            start_token_hex: identity.start_token_hex.clone(),
            checkout_hex: super::checkout_hex(checkout),
        }
    }

    #[test]
    fn liveness_requires_comparable_positive_process_evidence() {
        let checkout = Path::new("/tmp/tool");
        let identity = current_identity().unwrap();
        let record = process_record(std::process::id().to_string(), &identity, checkout);
        assert_eq!(record_liveness(&record), Liveness::Live);

        let mut reused = record.clone();
        reused.start_token_hex = if reused.start_token_hex == "30" {
            "31".to_owned()
        } else {
            "30".to_owned()
        };
        assert_eq!(record_liveness(&reused), Liveness::Dead);

        let mut different_backend = record.clone();
        different_backend.start_kind_hex = if identity.start_kind_hex == super::PROC_STAT_KIND_HEX {
            super::PS_LSTART_KIND_HEX.to_owned()
        } else {
            super::PROC_STAT_KIND_HEX.to_owned()
        };
        assert_eq!(record_liveness(&different_backend), Liveness::Unknown);

        let mut remote = record.clone();
        remote.host_hex = if remote.host_hex == "00" {
            "01".to_owned()
        } else {
            "00".to_owned()
        };
        assert_eq!(record_liveness(&remote), Liveness::Unknown);

        let mut oversized = record;
        oversized.pid = "999999999999999999999999999999".to_owned();
        assert_eq!(record_liveness(&oversized), Liveness::Dead);
    }

    #[test]
    fn actual_zombie_is_positive_dead_owner_evidence() {
        let host_hex = hostname_hex().unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            unsafe { libc::_exit(0) };
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let observed = loop {
            if let Ok((start_kind_hex, start_token_hex, state)) = process_identity(pid) {
                if state == b'Z' {
                    break Some(ProcessIdentity {
                        host_hex: host_hex.clone(),
                        start_kind_hex,
                        start_token_hex,
                        state,
                    });
                }
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::yield_now();
        };
        let liveness = observed.as_ref().map(|identity| {
            let record = process_record(pid.to_string(), identity, Path::new("/tmp/tool"));
            record_liveness(&record)
        });
        let mut wait_status = 0;
        unsafe {
            libc::waitpid(pid, &mut wait_status, 0);
        }
        assert!(
            observed.is_some(),
            "child never became an observable zombie"
        );
        assert_eq!(liveness, Some(Liveness::Dead));
    }

    #[test]
    fn dead_owner_bearing_claim_is_taken_over_and_cleaned() {
        let requested = checkout("dead-claim");
        let paths = Paths::new(&requested).unwrap();
        let owner = prepare_owner(&paths).unwrap();
        symlink(&owner.target, &paths.canonical).unwrap();
        let claim = prepare_claim(&paths, &owner.nonce).unwrap();
        fs::rename(&owner.dir, claim.dir.join("owner")).unwrap();
        fs::remove_file(claim.dir.join("claim-v1")).unwrap();
        let identity = current_identity().unwrap();
        write_record(
            &claim.dir.join("claim-v1"),
            Role::Claim,
            &claim.nonce,
            Some(&owner.nonce),
            "2147483647",
            &identity,
            &paths.checkout,
        )
        .unwrap();

        let mut lock = CheckoutLock::acquire(&requested, Duration::from_secs(1)).unwrap();

        assert!(!path_exists(&claim.dir));
        assert_ne!(
            fs::read_link(&paths.canonical).unwrap().to_string_lossy(),
            owner.target
        );
        lock.release().unwrap();
    }

    #[test]
    fn concurrent_fresh_writers_never_enter_the_mutation_together() {
        let requested = checkout("two-writers");
        assert_eq!(race_two_acquirers(&requested), 1);
        let mut retry = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();
        retry.release().unwrap();
    }

    #[test]
    fn unpublished_owner_debris_is_inert_and_preserved() {
        let requested = checkout("prepublication-debris");
        let paths = Paths::new(&requested).unwrap();
        let abandoned = prepare_owner(&paths).unwrap();

        let mut lock = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();

        assert!(path_exists(&abandoned.dir));
        assert_ne!(
            fs::read_link(&paths.canonical).unwrap().to_string_lossy(),
            abandoned.target
        );
        lock.release().unwrap();
    }

    #[test]
    fn legacy_publication_winner_cannot_receive_a_nested_symlink() {
        let requested = checkout("legacy-publication-race");
        let paths = Paths::new(&requested).unwrap();
        let unpublished = prepare_owner(&paths).unwrap();
        fs::create_dir(&paths.canonical).unwrap();

        let error = symlink(&unpublished.target, &paths.canonical).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        discard_unpublished_owner(&unpublished).unwrap();

        assert_eq!(fs::read_dir(&paths.canonical).unwrap().count(), 0);
    }

    #[test]
    fn interrupted_empty_claim_is_inert_while_owner_remains_live() {
        let requested = checkout("empty-claim");
        let paths = Paths::new(&requested).unwrap();
        let mut owner = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();
        let generation = owner.owner.as_ref().unwrap().clone();
        let claim = prepare_claim(&paths, &generation.nonce).unwrap();

        let error = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(path_exists(&claim.dir));
        assert!(path_exists(&generation.dir));
        owner.release().unwrap();
        cleanup_empty_claim(&paths, &claim, &generation.nonce).unwrap();
    }

    #[test]
    fn contender_retries_successfully_after_live_owner_releases() {
        let requested = checkout("wait-to-success");
        let mut first = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (released_tx, released_rx) = mpsc::channel();
        let worker_path = requested.clone();
        let worker = thread::spawn(move || {
            let blocked = CheckoutLock::acquire(&worker_path, Duration::ZERO).unwrap_err();
            assert_eq!(blocked.kind(), std::io::ErrorKind::TimedOut);
            blocked_tx.send(()).unwrap();
            released_rx.recv().unwrap();
            let mut second = CheckoutLock::acquire(&worker_path, Duration::from_secs(10)).unwrap();
            second.release().unwrap();
        });

        blocked_rx.recv().unwrap();
        first.release().unwrap();
        released_tx.send(()).unwrap();
        worker.join().unwrap();

        assert!(!path_exists(&Paths::new(&requested).unwrap().canonical));
    }

    #[test]
    fn two_stale_reclaimers_elect_serial_generations() {
        let requested = checkout("two-stale-reclaimers");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let stale_owner = owner_state(
            &paths,
            owner_nonce,
            "2147483647",
            &current_identity().unwrap(),
        );
        symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();
        assert_eq!(race_two_acquirers(&requested), 1);

        assert!(!path_exists(&stale_owner));
        assert!(!path_exists(&paths.canonical));
    }

    #[test]
    fn competing_detached_claim_cleaners_do_not_touch_new_generations() {
        let requested = checkout("two-detached-cleaners");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let claim_dir = claim_state(
            &paths,
            owner_nonce,
            "11111111111111111111111111111111",
            "2147483647",
            &current_identity().unwrap(),
            &current_identity().unwrap(),
            true,
        );
        assert_eq!(race_two_acquirers(&requested), 1);

        assert!(!path_exists(&claim_dir));
        assert!(!path_exists(&paths.canonical));
    }

    #[test]
    fn live_detached_claim_is_outside_arbitration_and_left_untouched() {
        let requested = checkout("live-detached-claim");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let claim_dir = claim_state(
            &paths,
            owner_nonce,
            "11111111111111111111111111111111",
            &std::process::id().to_string(),
            &current_identity().unwrap(),
            &current_identity().unwrap(),
            true,
        );

        let mut lock = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();

        assert!(path_exists(&claim_dir.join("owner/canonical")));
        assert!(path_exists(&claim_dir.join("claim-v1")));
        assert_ne!(
            fs::read_link(&paths.canonical).unwrap().to_string_lossy(),
            paths.owner_target(owner_nonce)
        );
        lock.release().unwrap();
        retire_and_cleanup_claim(&paths, &claim_dir, owner_nonce).unwrap();
    }

    #[test]
    fn interrupted_retired_leaf_cleanup_is_inert_during_fresh_acquisition() {
        let requested = checkout("retired-cleanup");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let claim = prepare_claim(&paths, owner_nonce).unwrap();
        let retired = claim.dir.join("retired");
        private_dir(&retired);
        write_record(
            &retired.join("owner-v1"),
            Role::Owner,
            owner_nonce,
            None,
            &std::process::id().to_string(),
            &current_identity().unwrap(),
            &paths.checkout,
        )
        .unwrap();
        symlink(paths.owner_target(owner_nonce), retired.join("canonical")).unwrap();

        let mut lock = CheckoutLock::acquire(&requested, Duration::ZERO).unwrap();

        assert!(path_exists(&retired.join("canonical")));
        assert!(path_exists(&retired.join("owner-v1")));
        lock.release().unwrap();
        fs::remove_file(retired.join("canonical")).unwrap();
        fs::remove_file(retired.join("owner-v1")).unwrap();
        fs::remove_dir(&retired).unwrap();
        fs::remove_file(claim.dir.join("claim-v1")).unwrap();
        fs::remove_dir(&claim.dir).unwrap();
    }

    #[test]
    fn non_utf8_owner_bearing_claim_fails_closed() {
        let requested = checkout("non-utf8-claim");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let mut name = paths.claim_prefix(owner_nonce).into_bytes();
        name.extend_from_slice(b"1111111111111111111111111111111");
        name.push(0xff);
        let claim_dir = paths.parent.join(OsString::from_vec(name));
        private_dir(&claim_dir);
        private_dir(&claim_dir.join("owner"));
        symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();

        assert!(matches!(classify(&paths), Classified::Malformed(_)));
    }

    #[test]
    fn positively_dead_owner_is_recovered_before_new_acquisition() {
        let requested = checkout("dead-owner");
        let paths = Paths::new(&requested).unwrap();
        let owner_nonce = "0123456789abcdef0123456789abcdef";
        let owner_dir = paths.owner_dir(owner_nonce);
        fs::create_dir(&owner_dir).unwrap();
        fs::set_permissions(&owner_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let identity = current_identity().unwrap();
        let dead_identity = ProcessIdentity {
            start_token_hex: "31".to_owned(),
            ..identity
        };
        write_record(
            &owner_dir.join("owner-v1"),
            Role::Owner,
            owner_nonce,
            None,
            "2147483647",
            &dead_identity,
            &paths.checkout,
        )
        .unwrap();
        symlink(paths.owner_target(owner_nonce), &paths.canonical).unwrap();

        let mut lock = CheckoutLock::acquire(&requested, Duration::from_secs(1)).unwrap();
        let new_target = fs::read_link(&paths.canonical).unwrap();

        assert_ne!(
            new_target.to_string_lossy(),
            paths.owner_target(owner_nonce)
        );
        assert!(!path_exists(&owner_dir));
        lock.release().unwrap();
    }
}
