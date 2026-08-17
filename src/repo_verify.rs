//! Repository verification capabilities used before Shdeps publishes state.
//!
//! Ordinary unrecorded adoption is the strict path: candidate metadata is only
//! a claim, so Git runs entirely inside an independent quarantine and never
//! receives candidate config, refs, object storage, or hooks. Explicit local
//! overrides supply only their separately configured remote path.
//!
//! An explicitly selected development checkout has a deliberately different
//! contract. It may be dirty and Git must inspect its local repository, but a
//! capability binds every such command to one followed root generation, one
//! trusted host Git executable, a sanitized environment, and a verified origin
//! plus tracked-command policy. Keeping the two contracts separate prevents a
//! future convenience change from weakening ordinary adoption.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::process::{self, Output, Runner};
use crate::repo;
use crate::repo_adopt::{self, OrdinaryCandidate};

const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);
const DEVELOPMENT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const DEVELOPMENT_PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 1_000_000;
static QUARANTINE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Capability proving one exact ordinary root matched its independent remote.
///
/// Fields stay private so callers cannot manufacture adoption authority from
/// an inert candidate claim or a successful but unrelated Git command.
#[derive(Debug)]
pub(crate) struct VerifiedOrdinary {
    candidate: OrdinaryCandidate,
}

impl VerifiedOrdinary {
    /// Consumes the capability only for the exact root generation it proved.
    pub(crate) fn authorize(self, root: &Path) -> io::Result<()> {
        if !self.candidate.matches_root(root)? {
            return Err(invalid("verified checkout root changed before adoption"));
        }
        Ok(())
    }
}

/// Successful proof or the one compatibility failure that keeps its existing
/// `MissingBinary` item classification at the update layer.
#[derive(Debug)]
pub(crate) enum Verification {
    /// The ordinary checkout is safe to adopt at the verified root.
    Verified(VerifiedOrdinary),
    /// The configured explicit command is not a tracked executable file.
    MissingCommand,
}

/// Capability proving that one selected development checkout has the expected
/// repository identity and, when configured, a tracked regular command.
#[derive(Debug)]
pub(crate) struct VerifiedDevelopment {
    identity: FollowedRootIdentity,
    git: DevelopmentGit,
    origin: repo::OriginPolicy,
    command: Option<String>,
}

impl VerifiedDevelopment {
    /// Consumes the capability only for the exact followed directory generation
    /// that was inspected. Dirty file contents may change by design.
    pub(crate) fn authorize(&self, root: &Path) -> io::Result<()> {
        if followed_root_identity(root)? != self.identity {
            return Err(invalid(
                "verified development checkout changed before publication",
            ));
        }
        Ok(())
    }

    /// Runs one Git operation against the exact development checkout and host
    /// executable selected during preparation. Callers intentionally receive
    /// the raw exit status because status/upstream/pull preserve their existing
    /// best-effort semantics.
    pub(crate) fn run_git(
        &self,
        root: &Path,
        runner: &impl Runner,
        args: &[&str],
    ) -> io::Result<Output> {
        self.authorize(root)?;
        self.git.run(runner, args, DEVELOPMENT_READ_TIMEOUT)
    }

    /// Runs the one potentially long development mutation with a separate
    /// bound. Repository reads should finish quickly; a network pull may
    /// legitimately wait for authentication or a slow remote.
    pub(crate) fn run_pull(&self, root: &Path, runner: &impl Runner) -> io::Result<Output> {
        self.authorize(root)?;
        self.git.run(
            runner,
            &["pull", "--ff-only", "--quiet"],
            DEVELOPMENT_PULL_TIMEOUT,
        )
    }

    /// Revalidates the mutable development properties immediately before
    /// publication. Pulls and ordinary developer edits are allowed between
    /// preparation and apply, so the initial proof alone cannot authorize the
    /// final origin/command shape.
    pub(crate) fn revalidate(self, root: &Path, runner: &impl Runner) -> io::Result<bool> {
        self.authorize(root)?;
        let command_available = validate_development_state(
            root,
            &self.git,
            &self.origin,
            self.command.as_deref(),
            runner,
        )?;
        if followed_root_identity(root)? != self.identity {
            return Err(invalid(
                "development checkout changed during final verification",
            ));
        }
        Ok(command_available)
    }
}

/// Result of the lighter trust rule for an explicitly selected development
/// source. Unlike ordinary adoption, this does not claim an exact revision.
#[derive(Debug)]
pub(crate) enum DevelopmentVerification {
    /// Origin and configured command policy passed for the selected root.
    Verified(VerifiedDevelopment),
    /// The explicitly configured command is not tracked as a regular 100755
    /// blob or the live worktree entry no longer has that shape.
    MissingCommand,
}

/// Inputs that define one development-checkout trust decision.
pub(crate) struct DevelopmentRequest<'a> {
    pub(crate) root: &'a Path,
    pub(crate) configured_origin: &'a str,
    pub(crate) command: &'a str,
    pub(crate) command_explicit: bool,
    pub(crate) env_vars: &'a BTreeMap<String, String>,
}

/// Inputs whose values define one ordinary-checkout proof attempt.
pub(crate) struct OrdinaryRequest<'a> {
    pub(crate) root: &'a Path,
    pub(crate) state_dir: &'a Path,
    pub(crate) approved_origin: &'a str,
    pub(crate) command: &'a str,
    pub(crate) command_explicit: bool,
    pub(crate) env_vars: &'a BTreeMap<String, String>,
    pub(crate) trusted_home: &'a Path,
}

/// Failure class used only to decide whether HTTPS may retry over SSH.
#[derive(Debug)]
pub(crate) struct VerificationError {
    error: io::Error,
    remote_access: bool,
}

impl VerificationError {
    fn rejected(error: io::Error) -> Self {
        Self {
            error,
            remote_access: false,
        }
    }

    fn remote_access(error: io::Error) -> Self {
        Self {
            error,
            remote_access: true,
        }
    }

    /// Returns whether only remote discovery/fetch failed before local proof.
    pub(crate) fn allows_ssh_fallback(&self) -> bool {
        self.remote_access
    }
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for VerificationError {}

/// Verifies an ordinary checkout before any live repository or transition
/// mutation and returns the capability required by the adoption path.
pub(crate) fn verify_ordinary(
    candidate: &OrdinaryCandidate,
    request: &OrdinaryRequest<'_>,
    runner: &impl Runner,
) -> std::result::Result<Verification, VerificationError> {
    let OrdinaryRequest {
        root,
        state_dir,
        approved_origin,
        command,
        command_explicit,
        env_vars,
        trusted_home,
    } = request;
    validate_candidate_metadata_tree(root).map_err(VerificationError::rejected)?;
    let physical_state =
        validate_quarantine_parent(root, state_dir).map_err(VerificationError::rejected)?;

    let quarantine = Quarantine::create(&physical_state).map_err(VerificationError::rejected)?;
    let git = CleanGit::new(
        runner,
        root,
        approved_origin,
        env_vars,
        trusted_home,
        &quarantine,
    )
    .map_err(VerificationError::rejected)?;
    let remote =
        discover_remote_head(&git, approved_origin).map_err(VerificationError::remote_access)?;
    if candidate.branch() != remote.branch {
        return Err(VerificationError::rejected(invalid(format!(
            "candidate branch `{}` is not remote default `{}`",
            candidate.branch(),
            remote.branch
        ))));
    }
    if candidate.head_oid() != remote.oid {
        return Err(VerificationError::rejected(invalid(
            "candidate HEAD does not match the remote default branch",
        )));
    }

    initialize_quarantine(&git, &quarantine).map_err(VerificationError::rejected)?;
    fetch_remote_head(&git, &quarantine, approved_origin, &remote)
        .map_err(VerificationError::remote_access)?;
    let fetched_commit =
        resolve_fetched_commit(&git, &quarantine).map_err(VerificationError::rejected)?;
    if fetched_commit != remote.oid || fetched_commit != candidate.head_oid() {
        return Err(VerificationError::rejected(invalid(
            "remote default moved or fetched identity did not match",
        )));
    }

    copy_candidate_index(root, &quarantine.candidate_index).map_err(VerificationError::rejected)?;
    verify_candidate_index(&git, &quarantine).map_err(VerificationError::rejected)?;
    let tree = read_remote_tree(&git, &quarantine).map_err(VerificationError::rejected)?;
    let command_available = validate_command_policy(&tree, command, *command_explicit)
        .map_err(VerificationError::rejected)?;
    verify_worktree(&git, &quarantine, root, &tree).map_err(VerificationError::rejected)?;

    // Re-run the inert parser after every remote and filesystem read. The
    // checkout lock excludes the installer and other Shdeps writers, while
    // this final comparison catches ordinary concurrent replacement before
    // the capability crosses into the mutating phase.
    let final_candidate = repo_adopt::inspect_with_policy(root, candidate.origin_policy())
        .map_err(VerificationError::rejected)?;
    if &final_candidate != candidate {
        return Err(VerificationError::rejected(invalid(
            "candidate changed during isolated verification",
        )));
    }
    validate_candidate_metadata_tree(root).map_err(VerificationError::rejected)?;

    if !command_available {
        return Ok(Verification::MissingCommand);
    }

    Ok(Verification::Verified(VerifiedOrdinary {
        candidate: final_candidate,
    }))
}

/// Verifies the narrow development-source contract without requiring a clean
/// index or worktree. Development checkout contents are deliberately live, but
/// their origin and published command ownership must not be inferred from a
/// directory name alone.
pub(crate) fn verify_development(
    request: &DevelopmentRequest<'_>,
    runner: &impl Runner,
) -> io::Result<DevelopmentVerification> {
    let DevelopmentRequest {
        root,
        configured_origin,
        command,
        command_explicit,
        env_vars,
    } = request;
    let identity = followed_root_identity(root)?;
    let origin = repo::OriginPolicy::new(configured_origin);
    let git = DevelopmentGit::new(runner, root, env_vars)?;
    let command = command_explicit.then(|| (*command).to_owned());
    let command_available =
        validate_development_state(root, &git, &origin, command.as_deref(), runner)?;
    if !command_available {
        return Ok(DevelopmentVerification::MissingCommand);
    }

    let final_identity = followed_root_identity(root)?;
    if final_identity != identity {
        return Err(invalid(
            "development checkout changed while it was being verified",
        ));
    }
    Ok(DevelopmentVerification::Verified(VerifiedDevelopment {
        identity: final_identity,
        git,
        origin,
        command,
    }))
}

// Recheck the mutable development-source contract: the selected path remains
// the repository root, both stored and Git-effective origins match policy, and
// an explicit command is tracked at HEAD and live as a regular executable.
// Checking both origins catches URL rewrites; checking HEAD plus the live path
// permits dirty tracked contents without trusting an untracked or symlinked
// command.
fn validate_development_state(
    root: &Path,
    git: &DevelopmentGit,
    origin_policy: &repo::OriginPolicy,
    command: Option<&str>,
    runner: &impl Runner,
) -> io::Result<bool> {
    let top_level = development_git_output(
        git,
        runner,
        &["rev-parse", "--show-toplevel"],
        "root lookup",
    )?;
    let top_level = one_output_line(&top_level, "development checkout root")?;
    if fs::canonicalize(top_level)? != git.cwd {
        return Err(invalid(
            "development checkout path is not the repository root",
        ));
    }
    let raw_origin = development_git_output(
        git,
        runner,
        &[
            "config",
            "--local",
            "--no-includes",
            "--get-all",
            "remote.origin.url",
        ],
        "origin lookup",
    )?;
    let raw_origin = one_output_line(&raw_origin, "development checkout origin")?;
    let effective_origin = development_git_output(
        git,
        runner,
        &["remote", "get-url", "--all", "origin"],
        "effective origin lookup",
    )?;
    let effective_origin =
        one_output_line(&effective_origin, "development checkout effective origin")?;
    if !origin_policy.matches(raw_origin) || !origin_policy.matches(effective_origin) {
        return Err(invalid(format!(
            "development checkout origin does not match `{}`",
            origin_policy.configured()
        )));
    }

    if let Some(command) = command {
        let command_path = format!("bin/{command}");
        let tree = development_git_output(
            git,
            runner,
            &["ls-tree", "-z", "--full-tree", "HEAD", "--", &command_path],
            "tracked command lookup",
        )?;
        if !development_tree_has_command(&tree, &command_path)
            || !live_development_command(root, command)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Reusable, environment-sanitized Git capability for one trusted dirty
/// development checkout. Unlike the ordinary verifier this intentionally lets
/// Git read the checkout's own local config and object database, while ambient
/// `GIT_DIR`, `GIT_WORK_TREE`, alternates, and executable selection cannot
/// redirect operations to another repository.
#[derive(Debug)]
struct DevelopmentGit {
    program: PathBuf,
    cwd: PathBuf,
    env: BTreeMap<OsString, OsString>,
}

impl DevelopmentGit {
    fn new(
        runner: &impl Runner,
        root: &Path,
        env_vars: &BTreeMap<String, String>,
    ) -> io::Result<Self> {
        let cwd = fs::canonicalize(root)?;
        require_development_metadata_root(&cwd)?;
        let program = trusted_program(runner, "git", &cwd)?;
        let mut env = BTreeMap::new();
        // Preserve the trusted developer's networking and authentication
        // environment, but only through this whitelist. Ambient GIT_* repo,
        // config, object-store, and executable selectors are intentionally
        // omitted so they cannot redirect verification away from `cwd`.
        for key in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "TMPDIR",
            "PATH",
            "SSH_AUTH_SOCK",
            "SSH_AGENT_PID",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "CURL_CA_BUNDLE",
            "USER",
            "LOGNAME",
        ] {
            if let Some(value) = env_vars.get(key) {
                env.insert(key.into(), value.into());
            }
        }
        env.insert("GIT_NO_REPLACE_OBJECTS".into(), "1".into());
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        env.insert("LC_ALL".into(), "C".into());
        env.insert("LANG".into(), "C".into());
        Ok(Self { program, cwd, env })
    }

    fn run(&self, runner: &impl Runner, args: &[&str], timeout: Duration) -> io::Result<Output> {
        let mut command = vec!["--no-pager".into(), "--no-replace-objects".into()];
        command.extend(args.iter().map(OsString::from));
        runner.run_env_clear(&self.program, &self.cwd, &command, &self.env, timeout)
    }
}

fn require_development_metadata_root(root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root.join(".git")).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid("development checkout path is not a repository root")
        } else {
            error
        }
    })?;
    if !metadata.file_type().is_dir() && !metadata.file_type().is_file() {
        return Err(invalid(
            "development checkout .git entry is linked or special",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FollowedRootIdentity {
    canonical: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn followed_root_identity(root: &Path) -> io::Result<FollowedRootIdentity> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(invalid("development checkout is not a directory"));
    }
    Ok(FollowedRootIdentity {
        canonical: fs::canonicalize(root)?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn development_git_output(
    git: &DevelopmentGit,
    runner: &impl Runner,
    args: &[&str],
    label: &str,
) -> io::Result<String> {
    let output = git.run(runner, args, DEVELOPMENT_READ_TIMEOUT)?;
    if output.timed_out {
        return Err(invalid(format!("development checkout {label} timed out")));
    }
    if !output.success {
        return Err(invalid(format!("development checkout {label} failed")));
    }
    Ok(output.stdout)
}

fn one_output_line<'a>(output: &'a str, label: &str) -> io::Result<&'a str> {
    let line = output.strip_suffix('\n').unwrap_or(output);
    if line.is_empty() || line.contains(['\n', '\r', '\0']) {
        return Err(invalid(format!("{label} is not one clean line")));
    }
    Ok(line)
}

fn development_tree_has_command(output: &str, expected_path: &str) -> bool {
    let Some(record) = output.strip_suffix('\0') else {
        return false;
    };
    if record.contains('\0') {
        return false;
    }
    let Some((header, path)) = record.split_once('\t') else {
        return false;
    };
    let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
    fields.len() == 3
        && fields[0] == "100755"
        && fields[1] == "blob"
        && valid_development_oid(fields[2])
        && path == expected_path
}

fn valid_development_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn live_development_command(root: &Path, command: &str) -> io::Result<bool> {
    let bin = root.join("bin");
    let bin_metadata = match fs::symlink_metadata(&bin) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !bin_metadata.file_type().is_dir() {
        return Ok(false);
    }
    let metadata = match fs::symlink_metadata(bin.join(command)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    Ok(executable_bit(&metadata))
}

// Resolve the existing physical prefix without creating missing components so
// verification state can never be published inside the candidate it is meant
// to preserve. State-root symlinks are resolved before the containment check.
fn validate_quarantine_parent(candidate_root: &Path, state_dir: &Path) -> io::Result<PathBuf> {
    let candidate_root = fs::canonicalize(candidate_root)?;
    let state_dir = physical_path_without_creation(state_dir)?;
    if state_dir.starts_with(&candidate_root) {
        return Err(invalid(
            "isolated verification state directory is inside the candidate checkout",
        ));
    }
    Ok(state_dir)
}

fn physical_path_without_creation(path: &Path) -> io::Result<PathBuf> {
    let mut cursor = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&cursor).is_ok() {
                    return Err(invalid(
                        "isolated verification state path contains a dangling link",
                    ));
                }
                let component = cursor
                    .file_name()
                    .ok_or_else(|| invalid("cannot resolve verification state directory"))?
                    .to_owned();
                if component == OsStr::new(".") || component == OsStr::new("..") {
                    return Err(invalid(
                        "verification state directory is not lexically normalized",
                    ));
                }
                missing.push(component);
                cursor = cursor
                    .parent()
                    .ok_or_else(|| invalid("cannot resolve verification state directory"))?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug)]
struct RemoteHead {
    branch: String,
    oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeKind {
    Regular { executable: bool },
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    kind: TreeKind,
    oid: String,
    size: u64,
}

#[derive(Debug)]
struct Quarantine {
    root: PathBuf,
    git_dir: PathBuf,
    home: PathBuf,
    xdg_config: PathBuf,
    xdg_data: PathBuf,
    xdg_cache: PathBuf,
    xdg_state: PathBuf,
    tmp: PathBuf,
    hooks: PathBuf,
    template: PathBuf,
    empty_config: PathBuf,
    candidate_index: PathBuf,
    scratch: PathBuf,
}

impl Quarantine {
    /// Creates one dependency-private verification namespace below Shdeps state.
    fn create(state_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let root = create_unique_private_dir(state_dir)?;
        let cleanup_root = root.clone();
        let result = (|| {
            let home = root.join("home");
            let xdg_config = root.join("xdg-config");
            let xdg_data = root.join("xdg-data");
            let xdg_cache = root.join("xdg-cache");
            let xdg_state = root.join("xdg-state");
            let tmp = root.join("tmp");
            let hooks = root.join("hooks");
            let template = root.join("template");
            for path in [
                &home,
                &xdg_config,
                &xdg_data,
                &xdg_cache,
                &xdg_state,
                &tmp,
                &hooks,
                &template,
            ] {
                create_private_dir(path)?;
            }
            let empty_config = root.join("empty.gitconfig");
            create_private_file(&empty_config)?;
            let candidate_index = root.join("candidate.index");
            let scratch = root.join("blob-input");
            Ok(Self {
                git_dir: root.join("repo.git"),
                root,
                home,
                xdg_config,
                xdg_data,
                xdg_cache,
                xdg_state,
                tmp,
                hooks,
                template,
                empty_config,
                candidate_index,
                scratch,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&cleanup_root);
        }
        result
    }
}

impl Drop for Quarantine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Runs only a canonical host Git with an explicit private environment.
struct CleanGit<'a, R> {
    runner: &'a R,
    program: PathBuf,
    cwd: PathBuf,
    env: BTreeMap<OsString, OsString>,
    common: Vec<OsString>,
}

impl<'a, R: Runner> CleanGit<'a, R> {
    /// Constructs the exact clean execution profile for one approved transport.
    fn new(
        runner: &'a R,
        candidate_root: &Path,
        origin: &str,
        env_vars: &BTreeMap<String, String>,
        trusted_home: &Path,
        quarantine: &Quarantine,
    ) -> io::Result<Self> {
        let candidate_root = fs::canonicalize(candidate_root)?;
        let program = trusted_program(runner, "git", &candidate_root)?;
        let program_parent = program
            .parent()
            .ok_or_else(|| invalid("isolated Git executable has no parent directory"))?;
        let mut executable_dirs = vec![program_parent.to_owned()];
        if runner.path("bash").is_some() {
            let bash = trusted_program(runner, "bash", &candidate_root)?;
            let bash_parent = bash
                .parent()
                .ok_or_else(|| invalid("isolated Bash executable has no parent directory"))?
                .to_owned();
            if !executable_dirs.contains(&bash_parent) {
                executable_dirs.push(bash_parent);
            }
        }
        for directory in [Path::new("/usr/bin"), Path::new("/bin")] {
            let directory = directory.to_owned();
            if !executable_dirs.contains(&directory) {
                executable_dirs.push(directory);
            }
        }
        let executable_path = std::env::join_paths(executable_dirs.iter())
            .map_err(|_| invalid("isolated executable path is not representable"))?;

        let mut env = BTreeMap::new();
        // Git or a host-selected wrapper may dispatch a helper through
        // `#!/usr/bin/env bash`. Preserve environment isolation while making
        // only the validated Git and Bash directories plus standard immutable
        // system directories discoverable; never admit the caller or checkout
        // PATH. The separate Bash directory matters on Termux when a client
        // wrapper selects Git outside the shared $PREFIX/bin tool root.
        env.insert("PATH".into(), executable_path);
        insert_env(&mut env, "HOME", &quarantine.home);
        insert_env(&mut env, "XDG_CONFIG_HOME", &quarantine.xdg_config);
        insert_env(&mut env, "XDG_DATA_HOME", &quarantine.xdg_data);
        insert_env(&mut env, "XDG_CACHE_HOME", &quarantine.xdg_cache);
        insert_env(&mut env, "XDG_STATE_HOME", &quarantine.xdg_state);
        insert_env(&mut env, "TMPDIR", &quarantine.tmp);
        insert_env(&mut env, "GIT_CONFIG_GLOBAL", &quarantine.empty_config);
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        env.insert("GIT_ATTR_NOSYSTEM".into(), "1".into());
        env.insert("GIT_PROTOCOL_FROM_USER".into(), "0".into());
        env.insert("GIT_NO_REPLACE_OBJECTS".into(), "1".into());
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        env.insert("LC_ALL".into(), "C".into());
        env.insert("LANG".into(), "C".into());
        insert_env(&mut env, "GIT_CEILING_DIRECTORIES", &quarantine.root);

        let origin_policy = repo::OriginPolicy::new(origin);
        let transport = match origin_policy.transport() {
            Some(repo::OriginTransport::Https) => "protocol.https.allow=always",
            Some(repo::OriginTransport::Ssh) => {
                configure_ssh(runner, &candidate_root, trusted_home, env_vars, &mut env)?;
                "protocol.ssh.allow=always"
            }
            Some(repo::OriginTransport::File) => {
                let remote_path = origin_policy.file_path().ok_or_else(|| {
                    invalid("configured local repository override is unavailable")
                })?;
                if remote_path.starts_with(&candidate_root) {
                    return Err(invalid(
                        "configured local repository override is inside the candidate checkout",
                    ));
                }
                let remote = fs::canonicalize(remote_path)
                    .map_err(|_| invalid("configured local repository override is unavailable"))?;
                if remote.starts_with(&candidate_root) {
                    return Err(invalid(
                        "configured local repository override is inside the candidate checkout",
                    ));
                }
                "protocol.file.allow=always"
            }
            None => {
                return Err(invalid(
                    "configured repository override uses an unsupported adoption transport",
                ));
            }
        };
        let common = vec![
            "--no-pager".into(),
            "--no-replace-objects".into(),
            "-c".into(),
            OsString::from(format!("core.hooksPath={}", quarantine.hooks.display())),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "core.untrackedCache=false".into(),
            "-c".into(),
            "submodule.recurse=false".into(),
            "-c".into(),
            "fetch.recurseSubmodules=false".into(),
            "-c".into(),
            "credential.helper=".into(),
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
            "-c".into(),
            "fetch.fsckObjects=true".into(),
            "-c".into(),
            "transfer.fsckObjects=true".into(),
            "-c".into(),
            "protocol.allow=never".into(),
            "-c".into(),
            transport.into(),
        ];
        Ok(Self {
            runner,
            program,
            cwd: quarantine.root.clone(),
            env,
            common,
        })
    }

    /// Executes one Git subcommand with optional private per-call environment.
    fn run(
        &self,
        args: impl IntoIterator<Item = OsString>,
        extra_env: &[(OsString, OsString)],
        label: &str,
    ) -> io::Result<String> {
        let mut command = self.common.clone();
        command.extend(args);
        let mut env = self.env.clone();
        env.extend(extra_env.iter().cloned());
        let output =
            self.runner
                .run_env_clear(&self.program, &self.cwd, &command, &env, VERIFY_TIMEOUT)?;
        successful_output(output, label)
    }
}

/// Parses the remote symbolic HEAD without trusting candidate refs or config.
fn discover_remote_head(git: &CleanGit<'_, impl Runner>, origin: &str) -> io::Result<RemoteHead> {
    let output = git.run(
        [
            "ls-remote".into(),
            "--symref".into(),
            "--exit-code".into(),
            "--".into(),
            origin.into(),
            "HEAD".into(),
        ],
        &[],
        "remote default discovery",
    )?;
    parse_remote_head(&output)
}

/// Initializes the bare quarantine without system or user templates.
fn initialize_quarantine(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
) -> io::Result<()> {
    git.run(
        [
            "-c".into(),
            OsString::from(format!(
                "init.templateDir={}",
                quarantine.template.display()
            )),
            "init".into(),
            "--quiet".into(),
            "--bare".into(),
            "--".into(),
            quarantine.git_dir.as_os_str().to_owned(),
        ],
        &[],
        "quarantine initialization",
    )?;
    Ok(())
}

/// Fetches exactly the independently discovered default branch and no tags.
fn fetch_remote_head(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
    origin: &str,
    remote: &RemoteHead,
) -> io::Result<()> {
    let refspec = format!("+refs/heads/{}:refs/heads/shdeps-adopt", remote.branch);
    git.run(
        [
            git_dir_arg(&quarantine.git_dir),
            "fetch".into(),
            "--quiet".into(),
            "--force".into(),
            "--no-tags".into(),
            "--depth=1".into(),
            "--".into(),
            origin.into(),
            refspec.into(),
        ],
        &[],
        "remote fetch",
    )?;
    Ok(())
}

/// Resolves the fetched ref as a commit entirely inside the quarantine.
fn resolve_fetched_commit(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
) -> io::Result<String> {
    let commit = git.run(
        [
            git_dir_arg(&quarantine.git_dir),
            "rev-parse".into(),
            "--verify".into(),
            "refs/heads/shdeps-adopt^{commit}".into(),
        ],
        &[],
        "fetched commit resolution",
    )?;
    let commit = commit.trim();
    if !valid_oid(commit) {
        return Err(invalid("quarantine returned a malformed commit identity"));
    }
    Ok(commit.to_owned())
}

/// Compares the copied candidate index with the independently fetched commit.
fn verify_candidate_index(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
) -> io::Result<()> {
    git.run(
        [
            git_dir_arg(&quarantine.git_dir),
            "diff-index".into(),
            "--cached".into(),
            "--quiet".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "refs/heads/shdeps-adopt".into(),
            "--".into(),
        ],
        &[(
            "GIT_INDEX_FILE".into(),
            quarantine.candidate_index.as_os_str().to_owned(),
        )],
        "candidate index comparison",
    )?;
    Ok(())
}

/// Loads a bounded remote tree manifest and rejects unsupported Git object
/// types before any candidate filesystem path is considered.
fn read_remote_tree(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
) -> io::Result<BTreeMap<String, TreeEntry>> {
    let output = git.run(
        [
            git_dir_arg(&quarantine.git_dir),
            "ls-tree".into(),
            "-l".into(),
            "-r".into(),
            "-z".into(),
            "--full-tree".into(),
            "refs/heads/shdeps-adopt".into(),
        ],
        &[],
        "remote tree listing",
    )?;
    parse_tree(&output)
}

/// Enforces the reusable explicit-command rule without a repository-specific
/// exception for dot or any other consumer.
fn validate_command_policy(
    tree: &BTreeMap<String, TreeEntry>,
    command: &str,
    command_explicit: bool,
) -> io::Result<bool> {
    let direct_commands = tree
        .iter()
        .filter(|(path, entry)| {
            path.strip_prefix("bin/")
                .is_some_and(|name| !name.is_empty() && !name.contains('/'))
                && matches!(
                    entry.kind,
                    TreeKind::Regular { executable: true } | TreeKind::Symlink
                )
        })
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    if !direct_commands.is_empty() && !command_explicit {
        return Err(invalid(
            "existing command-bearing checkout requires an explicit command column",
        ));
    }
    if command_explicit {
        let expected = format!("bin/{command}");
        return Ok(tree
            .get(&expected)
            .is_some_and(|entry| entry.kind == (TreeKind::Regular { executable: true })));
    }
    Ok(true)
}

/// Compares every fetched tree entry and rejects every extra candidate object.
fn verify_worktree(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
    root: &Path,
    tree: &BTreeMap<String, TreeEntry>,
) -> io::Result<()> {
    // Establish every traversed directory as a real in-root directory before
    // opening a leaf path. Otherwise a symlinked ancestor could make the
    // later leaf open read an outside file before the final extra-path scan.
    reject_extra_worktree_entries(root, tree.keys().map(String::as_str))?;
    for (relative, expected) in tree {
        verify_tree_entry(git, quarantine, root, relative, expected)?;
    }
    Ok(())
}

/// Verifies one candidate path by hashing only a private copy of its bytes.
fn verify_tree_entry(
    git: &CleanGit<'_, impl Runner>,
    quarantine: &Quarantine,
    root: &Path,
    relative: &str,
    expected: &TreeEntry,
) -> io::Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid(format!("candidate is missing tracked path `{relative}`"))
        } else {
            error
        }
    })?;
    match expected.kind {
        TreeKind::Regular { executable } => {
            if !metadata.file_type().is_file() {
                return Err(invalid(format!(
                    "candidate tracked file `{relative}` has the wrong type"
                )));
            }
            reject_multiple_links(&metadata, relative)?;
            if metadata.len() != expected.size {
                return Err(invalid(format!(
                    "candidate tracked file `{relative}` has the wrong size"
                )));
            }
            if executable_bit(&metadata) != executable {
                return Err(invalid(format!(
                    "candidate tracked file `{relative}` has the wrong executable mode"
                )));
            }
            copy_open_regular(&path, &metadata, &quarantine.scratch, expected.size)?;
        }
        TreeKind::Symlink => {
            if !metadata.file_type().is_symlink() {
                return Err(invalid(format!(
                    "candidate tracked symlink `{relative}` has the wrong type"
                )));
            }
            write_symlink_target(&path, &quarantine.scratch, expected.size)?;
        }
    }
    let oid = git
        .run(
            [
                git_dir_arg(&quarantine.git_dir),
                "hash-object".into(),
                "--no-filters".into(),
                "--".into(),
                quarantine.scratch.as_os_str().to_owned(),
            ],
            &[],
            "candidate blob hashing",
        )?
        .trim()
        .to_owned();
    if oid != expected.oid {
        return Err(invalid(format!(
            "candidate tracked path `{relative}` does not match the fetched blob"
        )));
    }
    Ok(())
}

/// Rejects untracked files, links, and special objects while allowing empty
/// directories, which Git does not represent in a tree.
fn reject_extra_worktree_entries<'a>(
    root: &Path,
    expected: impl Iterator<Item = &'a str>,
) -> io::Result<()> {
    let expected = expected.collect::<BTreeSet<_>>();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0;
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            visited += 1;
            if visited > MAX_TREE_ENTRIES {
                return Err(invalid("candidate worktree is unexpectedly large"));
            }
            let entry = entry?;
            if directory == root && entry.file_name() == OsStr::new(".git") {
                continue;
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| invalid("candidate walk escaped its root"))?;
            let relative = safe_relative_utf8(relative)?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                return Err(invalid(format!(
                    "candidate contains special path `{relative}`"
                )));
            }
            if !expected.contains(relative.as_str()) {
                return Err(invalid(format!(
                    "candidate contains untracked path `{relative}`"
                )));
            }
            if file_type.is_file() {
                reject_multiple_links(&entry.metadata()?, &relative)?;
            }
        }
    }
    Ok(())
}

/// Rejects every link, special file, or hardlinked regular inode inside .git so
/// later verified Git/config/permission work cannot escape the managed root.
fn validate_candidate_metadata_tree(root: &Path) -> io::Result<()> {
    let git_dir = root.join(".git");
    let mut stack = vec![git_dir.clone()];
    let mut visited = 0;
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            visited += 1;
            if visited > MAX_TREE_ENTRIES {
                return Err(invalid("candidate Git metadata is unexpectedly large"));
            }
            let entry = entry?;
            let file_type = entry.file_type()?;
            let relative = entry
                .path()
                .strip_prefix(&git_dir)
                .map_err(|_| invalid("candidate metadata walk escaped .git"))?
                .to_path_buf();
            let relative = safe_relative_utf8(&relative)?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                reject_multiple_links(&entry.metadata()?, &format!(".git/{relative}"))?;
            } else {
                return Err(invalid(format!(
                    "candidate Git metadata contains linked or special path `.git/{relative}`"
                )));
            }
        }
    }
    Ok(())
}

/// Copies the bounded candidate index without granting its original path to Git.
fn copy_candidate_index(root: &Path, destination: &Path) -> io::Result<()> {
    let source = root.join(".git/index");
    let metadata = fs::symlink_metadata(&source)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_INDEX_BYTES {
        return Err(invalid(
            "candidate index is missing, special, or unexpectedly large",
        ));
    }
    reject_multiple_links(&metadata, ".git/index")?;
    copy_open_regular(&source, &metadata, destination, MAX_INDEX_BYTES)
}

/// Copies one already-lstat'd regular inode to a private file and rejects a
/// swap between the no-follow metadata check and descriptor acquisition.
fn copy_open_regular(
    source: &Path,
    expected: &fs::Metadata,
    destination: &Path,
    limit: u64,
) -> io::Result<()> {
    #[cfg(unix)]
    let source_file = {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        options.open(source)?
    };
    #[cfg(not(unix))]
    let source_file = File::open(source)?;
    let opened = source_file.metadata()?;
    if !opened.is_file() || !same_file(expected, &opened) {
        return Err(invalid("candidate file changed while it was being opened"));
    }
    let mut destination_file = private_output_file(destination)?;
    let copied = io::copy(
        &mut source_file.take(limit.saturating_add(1)),
        &mut destination_file,
    )?;
    if copied != expected.len() || copied > limit {
        return Err(invalid("candidate file changed size while it was copied"));
    }
    destination_file.sync_all()?;
    Ok(())
}

/// Copies raw symlink-target bytes into quarantine for Git blob hashing.
fn write_symlink_target(source: &Path, destination: &Path, expected_size: u64) -> io::Result<()> {
    let target = fs::read_link(source)?;
    let mut file = private_output_file(destination)?;
    #[cfg(unix)]
    {
        let bytes = target.as_os_str().as_bytes();
        if u64::try_from(bytes.len()).ok() != Some(expected_size) {
            return Err(invalid("candidate symlink target has the wrong size"));
        }
        file.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        let target = target.to_string_lossy();
        let bytes = target.as_bytes();
        if u64::try_from(bytes.len()).ok() != Some(expected_size) {
            return Err(invalid("candidate symlink target has the wrong size"));
        }
        file.write_all(bytes)?;
    }
    file.sync_all()?;
    Ok(())
}

/// Opens or truncates one private scratch/output file with restrictive mode.
fn private_output_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

/// Returns whether Git should observe an executable bit for one regular file.
fn executable_bit(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// Compares descriptor identity with the no-follow metadata snapshot.
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len()
    }
}

/// Rejects regular inodes aliased outside the candidate ownership boundary.
fn reject_multiple_links(metadata: &fs::Metadata, label: &str) -> io::Result<()> {
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(invalid(format!(
            "candidate path `{label}` has hardlink aliases"
        )));
    }
    #[cfg(not(unix))]
    let _ = (metadata, label);
    Ok(())
}

/// Parses the two-line ls-remote symbolic HEAD contract.
fn parse_remote_head(output: &str) -> io::Result<RemoteHead> {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() != 2 {
        return Err(invalid("remote HEAD response is malformed"));
    }
    let branch = lines[0]
        .strip_prefix("ref: refs/heads/")
        .and_then(|line| line.strip_suffix("\tHEAD"))
        .filter(|branch| valid_ref_suffix(branch))
        .ok_or_else(|| invalid("remote HEAD is not a safe symbolic branch"))?;
    let oid = lines[1]
        .strip_suffix("\tHEAD")
        .filter(|oid| valid_oid(oid))
        .ok_or_else(|| invalid("remote HEAD object id is malformed"))?;
    Ok(RemoteHead {
        branch: branch.to_owned(),
        oid: oid.to_ascii_lowercase(),
    })
}

/// Parses the bounded NUL-delimited ls-tree wire output.
fn parse_tree(output: &str) -> io::Result<BTreeMap<String, TreeEntry>> {
    if output.len() > MAX_TREE_OUTPUT_BYTES || output.contains('\u{fffd}') {
        return Err(invalid("remote tree output is too large or non-UTF-8"));
    }
    if !output.is_empty() && !output.ends_with('\0') {
        return Err(invalid("remote tree output is missing its NUL terminator"));
    }
    let mut tree = BTreeMap::new();
    for (index, record) in output.split_terminator('\0').enumerate() {
        if index >= MAX_TREE_ENTRIES {
            return Err(invalid("remote tree has too many entries"));
        }
        let (header, path) = record
            .split_once('\t')
            .ok_or_else(|| invalid("remote tree record is malformed"))?;
        let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 || fields[1] != "blob" || !valid_oid(fields[2]) {
            return Err(invalid("remote tree contains unsupported object type"));
        }
        let size = fields[3]
            .parse::<u64>()
            .map_err(|_| invalid("remote tree contains an invalid blob size"))?;
        let kind = match fields[0] {
            "100644" => TreeKind::Regular { executable: false },
            "100755" => TreeKind::Regular { executable: true },
            "120000" => TreeKind::Symlink,
            _ => return Err(invalid("remote tree contains unsupported mode")),
        };
        safe_relative_utf8(Path::new(path))?;
        if tree
            .insert(
                path.to_owned(),
                TreeEntry {
                    kind,
                    oid: fields[2].to_ascii_lowercase(),
                    size,
                },
            )
            .is_some()
        {
            return Err(invalid("remote tree repeats a path"));
        }
    }
    Ok(tree)
}

/// Validates one relative UTF-8 path without allowing navigation components.
fn safe_relative_utf8(path: &Path) -> io::Result<String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid("repository tree contains an unsafe path"));
    }
    let path = path
        .to_str()
        .ok_or_else(|| invalid("repository tree contains a non-UTF-8 path"))?;
    if path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part.eq_ignore_ascii_case(".git"))
    {
        return Err(invalid("repository tree contains a reserved path"));
    }
    Ok(path.to_owned())
}

/// Restricts remote branch names to Git's safe ref grammar.
fn valid_ref_suffix(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.ends_with(['.', '/'])
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name.split('/').all(|component| {
            !component.is_empty()
                && !component.starts_with('.')
                && !component.ends_with(".lock")
                && component.bytes().all(|byte| {
                    !byte.is_ascii_control()
                        && byte != b' '
                        && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
                })
        })
}

/// Accepts only the SHA-1 object grammar supported by adoption v1.
fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Converts a completed clean subprocess into bounded diagnostic output.
fn successful_output(output: Output, label: &str) -> io::Result<String> {
    if output.timed_out {
        return Err(invalid(format!("isolated verification {label} timed out")));
    }
    if !output.success {
        let detail = output.stderr.trim();
        return Err(invalid(if detail.is_empty() {
            format!("isolated verification {label} failed")
        } else {
            format!("isolated verification {label} failed: {detail}")
        }));
    }
    Ok(output.stdout)
}

/// Creates one unique private quarantine directory without following an
/// attacker-precreated final path.
fn create_unique_private_dir(parent: &Path) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let nonce = QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".repo-verify.{}.{}", std::process::id(), nonce));
        match create_private_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(invalid("cannot allocate a private repository quarantine"))
}

/// Creates a directory privately even under a permissive process umask.
fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

/// Creates one empty private file used only as inert Git configuration.
fn create_private_file(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).map(drop)
}

/// Inserts a path-valued variable into the explicit child environment.
fn insert_env(env: &mut BTreeMap<OsString, OsString>, key: &str, value: &Path) {
    env.insert(key.into(), value.as_os_str().to_owned());
}

/// Configures stock SSH without inheriting user config, forwarding, prompts,
/// or an unvalidated agent socket.
fn configure_ssh(
    runner: &impl Runner,
    candidate_root: &Path,
    trusted_home: &Path,
    env_vars: &BTreeMap<String, String>,
    env: &mut BTreeMap<OsString, OsString>,
) -> io::Result<()> {
    let ssh = trusted_program(runner, "ssh", candidate_root)?;
    let known_hosts = fs::canonicalize(trusted_home.join(".ssh/known_hosts")).map_err(|_| {
        invalid("isolated SSH verification requires a trusted ~/.ssh/known_hosts file")
    })?;
    if !fs::metadata(&known_hosts)?.is_file() || known_hosts.starts_with(candidate_root) {
        return Err(invalid(
            "isolated SSH verification known_hosts source is not trusted",
        ));
    }
    let command = format!(
        "{} -F /dev/null -oBatchMode=yes -oClearAllForwardings=yes -oForwardAgent=no -oForwardX11=no -oPermitLocalCommand=no -oStrictHostKeyChecking=yes -oUpdateHostKeys=no -oGlobalKnownHostsFile=/dev/null -oUserKnownHostsFile={}",
        shell_quote(ssh.as_os_str()),
        shell_quote(known_hosts.as_os_str())
    );
    env.insert("GIT_SSH_COMMAND".into(), command.into());
    if let Some(socket) = env_vars.get("SSH_AUTH_SOCK") {
        let requested_socket = Path::new(socket);
        if !requested_socket.is_absolute() {
            return Err(invalid(
                "SSH_AUTH_SOCK must be absolute for isolated verification",
            ));
        }
        let socket = fs::canonicalize(requested_socket)?;
        #[cfg(unix)]
        if !fs::metadata(&socket)?.file_type().is_socket() {
            return Err(invalid("SSH_AUTH_SOCK is not a socket"));
        }
        if socket.starts_with(candidate_root) {
            return Err(invalid(
                "SSH_AUTH_SOCK must be outside the candidate checkout",
            ));
        }
        env.insert("SSH_AUTH_SOCK".into(), socket.into_os_string());
    }
    Ok(())
}

/// Resolves a host tool without allowing PATH to select even a symlink entry
/// from the untrusted candidate. Checking the selected directory before the
/// final canonicalization matters: otherwise `candidate/bin/git -> /usr/bin/git`
/// would appear trusted after resolution even though candidate state selected
/// what Shdeps executed.
fn trusted_program(
    runner: &impl Runner,
    command: &str,
    candidate_root: &Path,
) -> io::Result<PathBuf> {
    let configured = runner.path(command).ok_or_else(|| {
        invalid(format!(
            "isolated verification requires a host `{command}` executable"
        ))
    })?;
    let configured = if configured.is_absolute() {
        configured
    } else {
        std::env::current_dir()?.join(configured)
    };
    // Reject the selected spelling before resolving any component. A PATH
    // entry such as `candidate/tools-link` may itself point at `/usr/bin`; if
    // we canonicalized its parent first, candidate-controlled selection would
    // disappear from the path even though it chose the executable.
    if configured.starts_with(candidate_root) {
        return Err(invalid(format!(
            "isolated verification `{command}` was selected from the candidate checkout"
        )));
    }
    let name = configured
        .file_name()
        .ok_or_else(|| invalid(format!("isolated `{command}` path has no file name")))?;
    let selected_parent = fs::canonicalize(
        configured
            .parent()
            .ok_or_else(|| invalid(format!("isolated `{command}` path has no parent")))?,
    )?;
    let selected = selected_parent.join(name);
    if selected.starts_with(candidate_root) {
        return Err(invalid(format!(
            "isolated verification `{command}` was selected from the candidate checkout"
        )));
    }

    let program = fs::canonicalize(&selected)?;
    if !program.is_absolute()
        || !process::executable_path(&program)
        || program.starts_with(candidate_root)
    {
        return Err(invalid(format!(
            "isolated verification `{command}` executable is not trusted"
        )));
    }
    Ok(program)
}

/// Single-quotes one trusted local path for OpenSSH's shell-parsed command.
fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Builds an explicit --git-dir argument without repository-selection env.
fn git_dir_arg(path: &Path) -> OsString {
    OsString::from(format!("--git-dir={}", path.display()))
}

/// Uses InvalidData consistently for fail-closed repository proof failures.
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use crate::process::{Output, Runner};
    use crate::{repo, repo_adopt};

    use super::{
        CleanGit, OrdinaryRequest, Quarantine, TreeEntry, TreeKind, Verification,
        copy_candidate_index, copy_open_regular, development_tree_has_command,
        initialize_quarantine, parse_remote_head, parse_tree, safe_relative_utf8,
        validate_command_policy, verify_candidate_index, verify_ordinary,
    };

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn remote_head_requires_one_symbolic_default_and_matching_oid_record() {
        let parsed =
            parse_remote_head(&format!("ref: refs/heads/main\tHEAD\n{OID}\tHEAD\n")).unwrap();
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.oid, OID);

        for malformed in [
            format!("{OID}\tHEAD\n"),
            format!("ref: refs/tags/v1\tHEAD\n{OID}\tHEAD\n"),
            format!("ref: refs/heads/main\tHEAD\n{OID}\tHEAD\nextra\n"),
            "ref: refs/heads/main\tHEAD\nnot-an-oid\tHEAD\n".to_owned(),
        ] {
            assert!(parse_remote_head(&malformed).is_err(), "{malformed:?}");
        }
    }

    #[test]
    fn tree_parser_accepts_only_supported_blobs_and_safe_paths() {
        let tree = parse_tree(&format!(
            "100755 blob {OID} 12\tbin/tool\0\
             100644 blob {OID} 8\tREADME.md\0\
             120000 blob {OID} 7\tcurrent\0"
        ))
        .unwrap();
        assert_eq!(
            tree.get("bin/tool"),
            Some(&TreeEntry {
                kind: TreeKind::Regular { executable: true },
                oid: OID.to_owned(),
                size: 12,
            })
        );

        for malformed in [
            format!("160000 commit {OID} -\tsubmodule\0"),
            format!("100644 blob {OID} 1\t../escape\0"),
            format!("100644 blob {OID} 1\tnested/.git/config\0"),
            format!("100644 blob {OID} 1\tnested/.GIT/config\0"),
            format!("100644 blob {OID} 1\tmissing-terminator"),
            format!(
                "100644 blob {OID} 1\tfile\0\
                     100755 blob {OID} 1\tfile\0"
            ),
        ] {
            assert!(parse_tree(&malformed).is_err(), "{malformed:?}");
        }
        assert!(safe_relative_utf8(std::path::Path::new(".git")).is_err());
    }

    #[test]
    fn command_policy_is_generic_and_preserves_missing_binary_classification() {
        let executable = TreeEntry {
            kind: TreeKind::Regular { executable: true },
            oid: OID.to_owned(),
            size: 1,
        };
        let asset = TreeEntry {
            kind: TreeKind::Regular { executable: false },
            oid: OID.to_owned(),
            size: 1,
        };

        let asset_only = BTreeMap::from([("plugin.zsh".to_owned(), asset.clone())]);
        assert!(validate_command_policy(&asset_only, "plugin", false).unwrap());

        let command_repo = BTreeMap::from([("bin/tool".to_owned(), executable)]);
        assert!(validate_command_policy(&command_repo, "tool", false).is_err());
        assert!(validate_command_policy(&command_repo, "tool", true).unwrap());
        assert!(!validate_command_policy(&command_repo, "other", true).unwrap());

        let symlink_command = BTreeMap::from([(
            "bin/tool".to_owned(),
            TreeEntry {
                kind: TreeKind::Symlink,
                oid: OID.to_owned(),
                size: 6,
            },
        )]);
        assert!(validate_command_policy(&symlink_command, "tool", false).is_err());
        assert!(!validate_command_policy(&symlink_command, "tool", true).unwrap());

        let non_executable = BTreeMap::from([("bin/tool".to_owned(), asset)]);
        assert!(!validate_command_policy(&non_executable, "tool", true).unwrap());
    }

    #[test]
    fn development_command_requires_one_exact_executable_blob_record() {
        assert!(development_tree_has_command(
            &format!("100755 blob {OID}\tbin/tool\0"),
            "bin/tool"
        ));
        assert!(development_tree_has_command(
            &format!("100755 blob {}\tbin/tool\0", "a".repeat(64)),
            "bin/tool"
        ));
        for output in [
            format!("100644 blob {OID}\tbin/tool\0"),
            format!("120000 blob {OID}\tbin/tool\0"),
            format!("100755 blob {OID}\tbin/other\0"),
            format!("100755 blob {OID}\tbin/tool\0extra\0"),
            format!("100755 tree {OID}\tbin/tool\0"),
            "".to_owned(),
        ] {
            assert!(!development_tree_has_command(&output, "bin/tool"));
        }
    }

    #[test]
    fn stock_git_accepts_quarantine_init_and_copied_index_comparison() {
        let root = super::create_unique_private_dir(&std::env::temp_dir()).unwrap();
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        fs::write(candidate.join("tracked"), "one\n").unwrap();
        fixture_git(&candidate, &["init", "--quiet"]);
        fixture_git(&candidate, &["add", "--all"]);
        fixture_git(
            &candidate,
            &[
                "-c",
                "user.name=Shdeps Test",
                "-c",
                "user.email=shdeps@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let remote = Command::new("git")
            .args([
                "ls-remote",
                "--symref",
                "--exit-code",
                "--",
                candidate.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            remote.status.success(),
            "stock Git rejected the isolated ls-remote argv shape: {}",
            String::from_utf8_lossy(&remote.stderr)
        );

        let quarantine = Quarantine::create(&root.join("state")).unwrap();
        let git = CleanGit::new(
            &crate::process::Process,
            &candidate,
            "https://github.com/owner/tool",
            &BTreeMap::new(),
            &root,
            &quarantine,
        )
        .unwrap();
        initialize_quarantine(&git, &quarantine).unwrap();
        fixture_git_dir(
            &quarantine.git_dir,
            &[
                "fetch",
                "--quiet",
                "--",
                candidate.to_str().unwrap(),
                "HEAD:refs/heads/shdeps-adopt",
            ],
        );

        copy_candidate_index(&candidate, &quarantine.candidate_index).unwrap();
        verify_candidate_index(&git, &quarantine).unwrap();

        fs::write(candidate.join("tracked"), "two\n").unwrap();
        fixture_git(&candidate, &["add", "--all"]);
        copy_candidate_index(&candidate, &quarantine.candidate_index).unwrap();
        assert!(verify_candidate_index(&git, &quarantine).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn isolated_git_can_resolve_a_separate_trusted_bash_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = super::create_unique_private_dir(&std::env::temp_dir()).unwrap();
        let source = root.join("source");
        let origin = root.join("origin.git");
        let host_bin = root.join("host-bin");
        let shell_bin = root.join("shell-bin");
        let git_wrapper = host_bin.join("git");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir_all(&host_bin).unwrap();
        fs::create_dir_all(&shell_bin).unwrap();

        let real_bash = crate::process::Process.path("bash").unwrap();
        fs::write(
            &git_wrapper,
            format!(
                "#!/usr/bin/env bash\nprintf 'ref: refs/heads/main\\tHEAD\\n{}\\tHEAD\\n'\n",
                OID
            ),
        )
        .unwrap();
        fs::set_permissions(&git_wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        fs::copy(&real_bash, shell_bin.join("bash")).unwrap();
        fs::set_permissions(shell_bin.join("bash"), fs::Permissions::from_mode(0o755)).unwrap();

        let quarantine = Quarantine::create(&root.join("state")).unwrap();
        let runner = CleanEnvRunner {
            git: git_wrapper.clone(),
            bash: shell_bin.join("bash"),
        };
        let origin_text = format!("file://{}", origin.display());
        let git = CleanGit::new(
            &runner,
            &source,
            &origin_text,
            &BTreeMap::new(),
            &root,
            &quarantine,
        )
        .unwrap();

        let physical_host_bin = fs::canonicalize(&host_bin).unwrap();
        let physical_shell_bin = fs::canonicalize(&shell_bin).unwrap();
        let expected_path = std::env::join_paths([
            physical_host_bin.as_os_str(),
            physical_shell_bin.as_os_str(),
            Path::new("/usr/bin").as_os_str(),
            Path::new("/bin").as_os_str(),
        ])
        .unwrap();
        assert_eq!(git.env.get(&OsString::from("PATH")), Some(&expected_path));
        super::discover_remote_head(&git, &origin_text).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stock_git_adopts_checkout_from_exact_local_origin_override() {
        let root = super::create_unique_private_dir(&std::env::temp_dir()).unwrap();
        let source = root.join("source");
        let origin = root.join("dot-origin.git");
        let candidate = root.join("candidate");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("README.md"), "locked checkout\n").unwrap();
        fixture_git(&source, &["init", "--quiet"]);
        fixture_git(&source, &["add", "--all"]);
        fixture_git(
            &source,
            &[
                "-c",
                "user.name=Shdeps Test",
                "-c",
                "user.email=shdeps@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        fixture_git(&source, &["branch", "-M", "main"]);

        let bare = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["clone", "--quiet", "--bare", "--"])
            .arg(&source)
            .arg(&origin)
            .output()
            .unwrap();
        assert!(
            bare.status.success(),
            "bare origin clone failed: {}",
            String::from_utf8_lossy(&bare.stderr)
        );
        let origin_text = format!("file://{}", origin.display());
        let checkout = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "clone",
                "--quiet",
                "--depth=1",
                "--single-branch",
                "--branch=main",
                "--no-tags",
                "--",
            ])
            .arg(&origin_text)
            .arg(&candidate)
            .output()
            .unwrap();
        assert!(
            checkout.status.success(),
            "candidate clone failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );

        let origin_policy = repo::OriginPolicy::new(&origin_text);
        let inspected = repo_adopt::inspect_with_policy(&candidate, &origin_policy).unwrap();
        let state_dir = root.join("state");
        let request = OrdinaryRequest {
            root: &candidate,
            state_dir: &state_dir,
            approved_origin: &origin_text,
            command: "tool",
            command_explicit: false,
            env_vars: &BTreeMap::new(),
            trusted_home: &root,
        };
        assert!(matches!(
            verify_ordinary(&inspected, &request, &crate::process::Process).unwrap(),
            Verification::Verified(_)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_override_cannot_select_the_untrusted_candidate_as_its_remote() {
        let root = super::create_unique_private_dir(&std::env::temp_dir()).unwrap();
        let candidate = root.join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        fixture_git(&candidate, &["init", "--quiet"]);
        let quarantine = Quarantine::create(&root.join("state")).unwrap();
        let origin = format!("file://{}", candidate.display());

        assert!(
            CleanGit::new(
                &crate::process::Process,
                &candidate,
                &origin,
                &BTreeMap::new(),
                &root,
                &quarantine,
            )
            .is_err()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn bounded_copy_rejects_a_regular_file_swapped_to_a_link_or_fifo() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let root = super::create_unique_private_dir(&std::env::temp_dir()).unwrap();
        let source = root.join("source");
        let outside = root.join("outside");
        let destination = root.join("destination");
        fs::write(&source, "trusted\n").unwrap();
        fs::write(&outside, "outside\n").unwrap();
        let expected = fs::symlink_metadata(&source).unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();
        assert!(copy_open_regular(&source, &expected, &destination, 1024).is_err());

        fs::remove_file(&source).unwrap();
        fs::write(&source, "trusted\n").unwrap();
        let expected = fs::symlink_metadata(&source).unwrap();
        fs::remove_file(&source).unwrap();
        let fifo = CString::new(source.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a live NUL-terminated path and mode has no special
        // bits. The test removes the path first so EEXIST cannot hide failure.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(copy_open_regular(&source, &expected, &destination, 1024).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    fn fixture_git(root: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "core.hooksPath=/dev/null", "-C"])
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git -C {} {} failed: {}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture_git_dir(git_dir: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg(format!("--git-dir={}", git_dir.display()))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git --git-dir={} {} failed: {}",
            git_dir.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    struct CleanEnvRunner {
        git: PathBuf,
        bash: PathBuf,
    }

    impl Runner for CleanEnvRunner {
        fn exists(&self, command: &str) -> bool {
            matches!(command, "git" | "bash")
        }

        fn path(&self, command: &str) -> Option<PathBuf> {
            match command {
                "git" => Some(self.git.clone()),
                "bash" => Some(self.bash.clone()),
                _ => None,
            }
        }

        fn run(
            &self,
            _program: &str,
            _args: &[&str],
            _timeout: Option<Duration>,
        ) -> std::io::Result<Output> {
            unreachable!("isolated verification must use run_env_clear")
        }

        fn run_env_clear(
            &self,
            program: &Path,
            cwd: &Path,
            args: &[OsString],
            env: &BTreeMap<OsString, OsString>,
            _timeout: Duration,
        ) -> std::io::Result<Output> {
            let output = Command::new(program)
                .current_dir(cwd)
                .args(args)
                .env_clear()
                .envs(env)
                .output()?;
            Ok(Output {
                success: output.status.success(),
                timed_out: false,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }
}
