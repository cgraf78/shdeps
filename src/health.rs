//! Read-only dependency health report behind `shdeps health`.
//!
//! Health callers such as `dot doctor` run interactively and beside a cron
//! `shdeps update`, so this module is deliberately stat-level: it reads the
//! config, the manifest, link ledgers, and small state records, and `lstat`s
//! the paths they name. It never contacts the network, runs hooks, probes
//! package managers, writes state, or takes the state lock. Files may change
//! underneath it; a vanished path reads as absent rather than as an error.
//!
//! The report format is a stable machine contract (one TSV row per problem,
//! see [`write_tsv`]); prose lives only in the final detail column.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::{self, Entry};
use crate::deferral_notice;
use crate::dep_links;
use crate::dep_path::ResolveError;
use crate::github_release_install::{self, UpgradeBlocker};
use crate::hooks;
use crate::link_state::{self, Kind as LinkKind};
use crate::manifest::{self, Manifest};
use crate::method;
use crate::platform::{self, RuntimeEnv};
use crate::runtime::Roots;
use crate::stale_remote;
use crate::state;
use crate::update_transition::{self, PendingVerdict};

/// Exit status when no problem was found.
pub const EXIT_HEALTHY: i32 = 0;
/// Exit status when at least one problem row was printed.
pub const EXIT_PROBLEMS: i32 = 1;
/// Exit status when some state could not be read (an `unreadable-state` row
/// is printed too). Deliberately not 2: older Shdeps exits 2 for the unknown
/// `health` command, which callers use to detect that it is unsupported.
pub const EXIT_UNREADABLE: i32 = 3;

/// How urgently a problem needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A command is broken, an upgrade will be refused, or state is unreadable.
    Fail,
    /// Degraded or pending state that a later run may repair.
    Warn,
}

impl Severity {
    /// Stable first-column token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Warn => "warn",
        }
    }
}

/// Stable problem vocabulary; each variant maps to one `kind` column token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProblemKind {
    /// An expected public command link is absent.
    MissingBinlink,
    /// A public command link points at a path that does not exist.
    DanglingBinlink,
    /// A public command resolves somewhere other than its expected target.
    WrongTarget,
    /// A public command resolves to a non-executable file.
    NotExecutable,
    /// A tracked man page or completion link dangles.
    DanglingLink,
    /// A configured repo/release/toolchain dependency has no install, or a
    /// `pkg` dependency's command is not on PATH.
    NotInstalled,
    /// `shdeps update` would refuse to upgrade a `github:release` root.
    InstallRootUnmanaged,
    /// An interrupted archive swap left a backup beside the install root.
    ArchiveBackup,
    /// A `post()` hook was deferred because it needed sudo without a terminal.
    DeferredPost,
    /// An `uninstall()` hook was deferred because it needed sudo without a terminal.
    DeferredUninstall,
    /// A `post()` hook has not completed and will be retried.
    PendingPost,
    /// An interrupted update or prune left recovery records behind.
    RecoveryState,
    /// A repo or release source has not refreshed from its remote for a day
    /// while its peers did, or its pull-failure streak spans a day.
    StaleRemote,
    /// An interrupted handoff that recovery refuses, so update or prune fails
    /// until an operator resolves it.
    BlockedTransition,
    /// An interrupted clone or release staging tree was left beside a root.
    TempTree,
    /// Config or state could not be read, so the report is incomplete.
    UnreadableState,
}

impl ProblemKind {
    /// Every kind, in token order, for documentation and contract tests.
    pub const ALL: [Self; 16] = [
        Self::MissingBinlink,
        Self::DanglingBinlink,
        Self::WrongTarget,
        Self::NotExecutable,
        Self::DanglingLink,
        Self::NotInstalled,
        Self::InstallRootUnmanaged,
        Self::ArchiveBackup,
        Self::DeferredPost,
        Self::DeferredUninstall,
        Self::PendingPost,
        Self::RecoveryState,
        Self::StaleRemote,
        Self::BlockedTransition,
        Self::TempTree,
        Self::UnreadableState,
    ];

    /// Stable `kind` column token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::MissingBinlink => "missing-binlink",
            Self::DanglingBinlink => "dangling-binlink",
            Self::WrongTarget => "wrong-target",
            Self::NotExecutable => "not-executable",
            Self::DanglingLink => "dangling-link",
            Self::NotInstalled => "not-installed",
            Self::InstallRootUnmanaged => "install-root-unmanaged",
            Self::ArchiveBackup => "archive-backup",
            Self::DeferredPost => "deferred-post",
            Self::DeferredUninstall => "deferred-uninstall",
            Self::PendingPost => "pending-post",
            Self::RecoveryState => "recovery-state",
            Self::StaleRemote => "stale-remote",
            Self::BlockedTransition => "blocked-transition",
            Self::TempTree => "temp-tree",
            Self::UnreadableState => "unreadable-state",
        }
    }

    /// Fixed severity: a kind never changes severity between runs, so callers
    /// may key policy on either column. `fail` is reserved for states the
    /// next `shdeps update` cannot repair by itself (it refuses the root,
    /// keeps a non-executable file, or cannot read state). Missing or
    /// dangling links only warn: they are usually transient (a development
    /// clone gains or drops a `bin/` script between cron runs) and never stop
    /// the command set from being repaired by hand.
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            Self::NotExecutable
            | Self::InstallRootUnmanaged
            | Self::BlockedTransition
            | Self::UnreadableState => Severity::Fail,
            Self::MissingBinlink
            | Self::DanglingBinlink
            | Self::WrongTarget
            | Self::DanglingLink
            | Self::NotInstalled
            | Self::ArchiveBackup
            | Self::DeferredPost
            | Self::DeferredUninstall
            | Self::PendingPost
            | Self::RecoveryState
            | Self::StaleRemote
            | Self::TempTree => Severity::Warn,
        }
    }

    /// Kinds an in-flight update or prune creates and retires itself (an
    /// archive swap's backup lives while the old tree is deleted, a clone
    /// lives in its temp tree until it is published). They are suppressed
    /// while the recorded lock owner is alive so a doctor run that races cron
    /// does not report healthy work in progress. A blocked transition is
    /// never transient: the running update fails on it too.
    const fn transient(self) -> bool {
        // A running update stamps sources one by one; until it finishes,
        // those not yet reached look stale next to those already done.
        matches!(
            self,
            Self::PendingPost
                | Self::RecoveryState
                | Self::ArchiveBackup
                | Self::StaleRemote
                | Self::TempTree
        )
    }
}

/// One detected problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Problem class.
    pub kind: ProblemKind,
    /// Dependency name, or `None` for state not owned by one dependency.
    pub package: Option<String>,
    /// Affected path, when there is one.
    pub path: Option<PathBuf>,
    /// One-line explanation ending in a remediation hint.
    pub detail: String,
}

/// Complete health report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Problems in stable output order.
    pub problems: Vec<Problem>,
}

impl Report {
    /// Maps the report to the documented exit status.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self
            .problems
            .iter()
            .any(|problem| problem.kind == ProblemKind::UnreadableState)
        {
            EXIT_UNREADABLE
        } else if self.problems.is_empty() {
            EXIT_HEALTHY
        } else {
            EXIT_PROBLEMS
        }
    }
}

/// Writes one `severity<TAB>package<TAB>kind<TAB>path<TAB>detail` row per
/// problem. Absent package/path columns are `-`; tabs and line breaks inside
/// a field are replaced with spaces so every row has exactly five fields.
pub fn write_tsv<W: Write>(report: &Report, writer: &mut W) -> Result<()> {
    for problem in &report.problems {
        let path = problem
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            problem.kind.severity().token(),
            field(problem.package.as_deref()),
            problem.kind.token(),
            field(path.as_deref()),
            field(Some(&problem.detail)),
        )?;
    }
    Ok(())
}

fn field(value: Option<&str>) -> String {
    match value {
        Some(value) if !value.is_empty() => value.replace(['\t', '\n', '\r'], " "),
        _ => "-".to_owned(),
    }
}

/// Builds the report for the configured roots and runtime identity.
///
/// `pkg_mgr` only resolves manager-qualified command names (`apt:batcat`) and
/// `mgr:` filters exactly as `update` does; no package manager is queried.
#[must_use]
pub fn check(roots: &Roots, env: &RuntimeEnv, pkg_mgr: &str) -> Report {
    let mut scan = Scan::new(roots);
    let entries = scan.configured_entries(env, pkg_mgr);
    // Without the manifest every install would look missing; report the
    // unreadable manifest alone instead of a cascade of guesses.
    let manifest = scan.manifest();
    if let Some(manifest) = &manifest {
        for entry in &entries {
            scan.check_entry(entry, manifest);
        }
        scan.check_transitions(&entries, manifest, env, pkg_mgr);
    }
    scan.check_pkg_commands(&entries, env, pkg_mgr);
    let manifest = manifest.unwrap_or_default();

    let mut ledger_names = entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<BTreeSet<_>>();
    ledger_names.extend(manifest.entries().iter().map(|row| row.name.clone()));
    // install.sh and self-update link Shdeps' own man page and completions
    // under this name without a manifest row.
    ledger_names.insert("shdeps".to_owned());
    for name in &ledger_names {
        scan.check_ledgers(name);
    }

    scan.check_deferrals();
    scan.check_recovery();
    scan.check_stale_remotes(&entries, &manifest);
    scan.finish()
}

/// Accumulates problems for one report.
///
/// It reports each (package, kind, path) once and remembers which public
/// commands were already inspected, so a command seen by both the dep-links
/// check and its ledger costs its syscalls once. Canonical
/// directories are cached because hundreds of commands share a few `bin/`s.
struct Scan<'a> {
    roots: &'a Roots,
    problems: Vec<Problem>,
    reported: BTreeSet<(Option<String>, ProblemKind, Option<PathBuf>)>,
    /// Whether every config file was read, so the entry list is complete.
    config_complete: bool,
    checked_links: BTreeSet<PathBuf>,
    canonical_dirs: BTreeMap<PathBuf, Option<PathBuf>>,
}

impl<'a> Scan<'a> {
    fn new(roots: &'a Roots) -> Self {
        Self {
            roots,
            problems: Vec::new(),
            reported: BTreeSet::new(),
            config_complete: true,
            checked_links: BTreeSet::new(),
            canonical_dirs: BTreeMap::new(),
        }
    }

    /// Physical spelling of `path`: its directory canonicalized (cached),
    /// its final component kept. This is the comparison the dep-links
    /// contract implies and `dot doctor` used: a link counts as correct when
    /// it names the expected file through any symlinked directory.
    fn physical(&mut self, path: &Path) -> Option<PathBuf> {
        let parent = path.parent()?;
        let name = path.file_name()?;
        let canonical = self
            .canonical_dirs
            .entry(parent.to_path_buf())
            .or_insert_with(|| fs::canonicalize(parent).ok())
            .clone()?;
        Some(canonical.join(name))
    }

    /// Whether the public link `public` names `expected`.
    fn resolves_to(&mut self, public: &Path, expected: &Path) -> bool {
        let Ok(raw) = fs::read_link(public) else {
            return false;
        };
        // Tracked archive links report their raw target verbatim.
        if raw == expected {
            return true;
        }
        let absolute = match public.parent() {
            Some(parent) if raw.is_relative() => parent.join(raw),
            _ => raw,
        };
        if absolute == expected {
            return true;
        }
        let physical = self.physical(&absolute);
        if physical.is_some() && physical == self.physical(expected) {
            return true;
        }
        // A chain of links (rare) still counts when it ends at the target.
        match (fs::canonicalize(public), fs::canonicalize(expected)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }

    fn push(
        &mut self,
        kind: ProblemKind,
        package: Option<&str>,
        path: Option<&Path>,
        detail: String,
    ) {
        let package = package.map(ToOwned::to_owned);
        let path = path.map(Path::to_path_buf);
        if !self.reported.insert((package.clone(), kind, path.clone())) {
            return;
        }
        self.problems.push(Problem {
            kind,
            package,
            path,
            detail,
        });
    }

    fn unreadable(&mut self, package: Option<&str>, path: &Path, error: impl std::fmt::Display) {
        self.push(
            ProblemKind::UnreadableState,
            package,
            Some(path),
            format!("cannot read: {error}; fix the file's permissions or move it aside"),
        );
    }

    /// Loads active config entries, deduplicated by name like `update`.
    fn configured_entries(&mut self, env: &RuntimeEnv, pkg_mgr: &str) -> Vec<Entry> {
        let conf_dir = self.roots.conf_dir.clone();
        let loaded = match config::load_dir_for_runtime_read_only(&conf_dir, env) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.unreadable(None, &conf_dir, error);
                self.config_complete = false;
                return Vec::new();
            }
        };
        if let Some(reason) = loaded.unreadable {
            self.unreadable(None, &conf_dir, reason);
            self.config_complete = false;
        }
        let pkg_mgr = (!pkg_mgr.is_empty()).then_some(pkg_mgr);
        let mut seen = BTreeSet::new();
        loaded
            .entries
            .iter()
            .map(|raw| config::parse_entry_for_runtime(raw, pkg_mgr, env.is_android()))
            .filter(|entry| {
                platform::filter_match(&entry.filter, env) == platform::FilterMatch::Match
            })
            .filter(|entry| seen.insert(entry.name.clone()))
            .collect()
    }

    fn manifest(&mut self) -> Option<Manifest> {
        let path = manifest::path(&self.roots.state_dir);
        match manifest::read(&path) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                self.unreadable(None, &path, error);
                None
            }
        }
    }

    /// Checks install presence, public command links, and release ownership
    /// for one configured dependency whose method has an owned install root.
    fn check_entry(&mut self, entry: &Entry, manifest: &Manifest) {
        let concrete = dep_links::concrete_method(&entry.method, &entry.name, manifest);
        if concrete != method::GITHUB_REPO && !method::is_binary_install_root(&concrete) {
            // pkg and custom dependencies own no root or links to inspect
            // without running package managers or hooks.
            return;
        }

        if concrete == method::GITHUB_RELEASE {
            self.check_release_root(entry, manifest);
        }

        // Every successful install writes a manifest row, so the row is the
        // install evidence. A bare development clone in SHDEPS_GIT_DEV_DIR is
        // not: for a bare `github` entry update may still choose a release.
        if manifest.get(&entry.name).is_none() {
            self.push(
                ProblemKind::NotInstalled,
                Some(&entry.name),
                None,
                format!(
                    "configured {} dependency is not installed; run 'shdeps update'",
                    entry.method
                ),
            );
            return;
        }
        // Only an archive release preserves a client's symlink launcher; the
        // other managed methods replace any symlink at their command paths.
        let release = concrete == method::GITHUB_RELEASE;
        match dep_links::links_for_entry(entry, self.roots, manifest) {
            Ok(links) => {
                for link in links {
                    self.check_public_link(&entry.name, &link, release);
                }
            }
            Err(crate::Error::Resolve(ResolveError::NotFound)) => self.push(
                ProblemKind::NotInstalled,
                Some(&entry.name),
                Some(&self.roots.install_dir.join(&entry.name)),
                format!("recorded {concrete} install has no checkout; run 'shdeps update'"),
            ),
            Err(error) => {
                // The `.binlinks` ledger (binary roots) or the checkout's
                // `bin/` (repos) could not be read.
                let path = if concrete == method::GITHUB_REPO {
                    self.roots.install_dir.join(&entry.name)
                } else {
                    link_state::path(&self.roots.state_dir, &entry.name, LinkKind::Bin)
                };
                self.unreadable(Some(&entry.name), &path, error);
            }
        }
    }

    /// Reports active `pkg` dependencies whose command is not on PATH.
    ///
    /// Update counts a package present when its command is in the bin dir
    /// or on PATH, and otherwise asks the package manager, which health
    /// must not do. A miss is therefore reported only when the command is
    /// known to be one: the last clean package scan found it, or, with no
    /// record, the config names it explicitly. A defaulted name may be no
    /// command at all (completion data, fonts), and `git-*` may live in
    /// Git's exec path. Packages update skips as unavailable are not
    /// reported (`pkg_unavailable`).
    fn check_pkg_commands(&mut self, entries: &[Entry], env: &RuntimeEnv, pkg_mgr: &str) {
        use crate::process::{self, Process, Runner};

        // Without a PATH every lookup misses, and without a detected
        // manager filters and package names resolve unlike update's; either
        // way a miss says nothing about the packages.
        if pkg_mgr.is_empty() || std::env::var_os("PATH").is_none_or(|path| path.is_empty()) {
            return;
        }
        let recorded = crate::package_cache::recorded_commands(&self.roots.state_dir, env, pkg_mgr)
            .unwrap_or_default();
        let unavailable = crate::pkg_unavailable::read(&self.roots.state_dir, env, pkg_mgr);
        for entry in entries.iter().filter(|entry| entry.method == method::PKG) {
            let package = config::resolve_override_for_runtime(
                &entry.name,
                &entry.aliases,
                Some(pkg_mgr),
                env.is_android(),
            );
            // Update skips, without failing, a package with a NONE override
            // on this manager and one its last full scan found unavailable
            // here; health must not warn about what update considers fine.
            // Deciding before the lookup keeps commandless packages from
            // walking every PATH entry (slow `/mnt/c` ones on WSL).
            let known_command = recorded
                .get(&entry.cmd)
                .copied()
                .unwrap_or(entry.cmd_explicit && !entry.cmd.starts_with("git-"));
            if package == "NONE" || unavailable.get(&entry.name) == Some(&package) || !known_command
            {
                continue;
            }
            // Update runs with the bin dir first on PATH (`update_cmd`), then
            // looks the command up exactly like this (`Runner::path`).
            let found = process::is_executable(&self.roots.bin_dir.join(&entry.cmd))
                || Runner::path(&Process, &entry.cmd).is_some();
            if !found {
                self.push(
                    ProblemKind::NotInstalled,
                    Some(&entry.name),
                    None,
                    format!(
                        "command '{}' from package '{package}' is not on PATH; run 'shdeps update', or reinstall '{package}' if the package manager still lists it",
                        entry.cmd
                    ),
                );
            }
        }
    }

    /// Verifies one public command against the dep-links contract.
    ///
    /// `release` marks a `github:release` dependency, whose update preserves a
    /// client's symlink launcher at the command path.
    fn check_public_link(
        &mut self,
        package: &str,
        link: &dep_links::DependencyLink,
        release: bool,
    ) {
        let public = &link.public_path;
        self.checked_links.insert(public.clone());
        let metadata = match fs::symlink_metadata(public) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Tracked archive links and raw binaries report the public
                // path itself as the target; naming it twice says nothing.
                let expected = if link.target_path == *public {
                    String::new()
                } else {
                    format!(" (expected -> {})", link.target_path.display())
                };
                self.push(
                    ProblemKind::MissingBinlink,
                    Some(package),
                    Some(public),
                    format!(
                        "command '{}' is missing{expected}; run 'shdeps update'",
                        link.command
                    ),
                );
                return;
            }
            Err(error) => {
                self.unreadable(Some(package), public, error);
                return;
            }
        };

        if metadata.file_type().is_symlink() {
            let target = match fs::metadata(public) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if release
                        && github_release_install::is_foreign_launcher(
                            &self.roots.state_dir,
                            &self.roots.install_dir,
                            package,
                            public,
                        )
                    {
                        self.push_dangling_launcher(package, link);
                    } else {
                        self.push_dangling(LinkKind::Bin, package, public);
                    }
                    return;
                }
                Err(error) => {
                    self.unreadable(Some(package), public, error);
                    return;
                }
            };
            // Update links straight to the target, so one readlink usually
            // settles it; canonicalization (a readlink per path component)
            // is cached per directory for links through symlinked roots such
            // as development clones.
            if *public != link.target_path && !self.resolves_to(public, &link.target_path) {
                let resolved = fs::canonicalize(public).unwrap_or_else(|_| public.clone());
                self.push(
                    ProblemKind::WrongTarget,
                    Some(package),
                    Some(public),
                    format!(
                        "command '{}' resolves to {}, expected {}; run 'shdeps update' or remove the conflicting link",
                        link.command,
                        resolved.display(),
                        link.target_path.display()
                    ),
                );
                return;
            }
            if !executable(&target) {
                self.push_not_executable(package, link);
            }
            return;
        }

        // A regular file is either the raw release binary itself or a
        // launcher a client deliberately put in front of the command; shdeps
        // preserves both, so only a broken (non-executable) one is a problem.
        if !executable(&metadata) {
            self.push_not_executable(package, link);
        }
    }

    fn push_not_executable(&mut self, package: &str, link: &dep_links::DependencyLink) {
        self.push(
            ProblemKind::NotExecutable,
            Some(package),
            Some(&link.public_path),
            format!(
                "command '{}' is not executable; run 'shdeps --reinstall update'",
                link.command
            ),
        );
    }

    /// Reports a broken client launcher link. `update` preserves links it did
    /// not create, so unlike a dangling Shdeps link it cannot repair this one.
    fn push_dangling_launcher(&mut self, package: &str, link: &dep_links::DependencyLink) {
        let public = &link.public_path;
        let target = fs::read_link(public).unwrap_or_default();
        self.push(
            ProblemKind::DanglingBinlink,
            Some(package),
            Some(public),
            format!(
                "command '{}' is a launcher link to missing {}; shdeps preserves links it did not create, so repair or remove it",
                link.command,
                target.display()
            ),
        );
    }

    fn push_dangling(&mut self, kind: LinkKind, package: &str, path: &Path) {
        let target = fs::read_link(path).unwrap_or_default();
        // `update` relinks extras only from an install root that still
        // exists, so a link left by a root that is gone for good (an archive
        // release that became a single binary) survives every update.
        // Removing a dangling link is always safe and a later update
        // recreates any extra the dependency still ships.
        let (problem, what, hint) = match kind {
            LinkKind::Bin => (
                ProblemKind::DanglingBinlink,
                "command link",
                "run 'shdeps update'",
            ),
            LinkKind::Extras => (
                ProblemKind::DanglingLink,
                "man/completion link",
                "remove the stale link",
            ),
        };
        self.push(
            problem,
            Some(package),
            Some(path),
            format!("{what} points to missing {}; {hint}", target.display()),
        );
    }

    /// Reports release roots the update path would refuse to upgrade, plus
    /// backups an interrupted archive swap left beside the root.
    fn check_release_root(&mut self, entry: &Entry, manifest: &Manifest) {
        let public = self.roots.bin_dir.join(&entry.cmd);
        let root = self.roots.install_dir.join(&entry.name);
        let prior_release = manifest
            .get(&entry.name)
            .is_some_and(|row| row.method == method::GITHUB_RELEASE);
        match github_release_install::upgrade_blocker(
            &self.roots.state_dir,
            &self.roots.install_dir,
            &public,
            &entry.name,
            prior_release,
        ) {
            Ok(None) => {}
            Ok(Some(blocker)) => {
                // Moving both the root and the public command aside is the
                // one remediation that always works: with neither present the
                // next update performs a fresh, fully owned install.
                let aside = format!(
                    "move {} and {} aside, then run 'shdeps update'",
                    root.display(),
                    public.display()
                );
                let detail = match blocker {
                    UpgradeBlocker::SymlinkedRoot { target } => format!(
                        "install root is a symlink to {}, which shdeps never follows, so upgrades are refused; {aside}",
                        target.display()
                    ),
                    UpgradeBlocker::NonDirectoryRoot => {
                        format!("install root is not a directory, so upgrades are refused; {aside}")
                    }
                    UpgradeBlocker::UnprovenRoot { ambiguous } => format!(
                        "install root lacks the {} marker{}, so upgrades are refused; {aside}",
                        github_release_install::ARCHIVE_LAYOUT_FILE,
                        if ambiguous {
                            " and the public command is a regular file"
                        } else {
                            ""
                        }
                    ),
                    UpgradeBlocker::MissingRoot => format!(
                        "install root is missing but the public command is still a link, so archive upgrades are refused; remove {} and run 'shdeps update'",
                        public.display()
                    ),
                    UpgradeBlocker::InvalidMarker(reason) => {
                        format!("{reason}, so updates fail; {aside}")
                    }
                    // Never advise moving an installer-owned root aside: the
                    // update adopts it once the lock is gone.
                    #[cfg(unix)]
                    UpgradeBlocker::InstallerLocked { lock } => format!(
                        "standalone installer lock {} blocks updates; remove it if no install.sh is running, then run 'shdeps update'",
                        lock.display()
                    ),
                };
                self.push(
                    ProblemKind::InstallRootUnmanaged,
                    Some(&entry.name),
                    Some(&root),
                    detail,
                );
            }
            Err(error) => self.unreadable(Some(&entry.name), &root, error),
        }

        for backup in github_release_install::archive_backups(&self.roots.install_dir, &entry.name)
        {
            self.push(
                ProblemKind::ArchiveBackup,
                Some(&entry.name),
                Some(&backup),
                "an interrupted archive update left this backup; remove it once the install works"
                    .to_owned(),
            );
        }
    }

    /// Reports interrupted ownership handoffs the next update must finish:
    /// method-transition records, public-command exchanges, and checkout
    /// journals. Each is classified the way recovery will treat it, so a
    /// record recovery refuses (aborting every update or that package) is a
    /// `blocked-transition` fail naming the package and the record, while one
    /// recovery finishes by itself stays a `recovery-state` warning.
    fn check_transitions(
        &mut self,
        entries: &[Entry],
        manifest: &Manifest,
        env: &RuntimeEnv,
        pkg_mgr: &str,
    ) {
        // Update hands recovery its entries with bare `github` resolved.
        let resolved = entries
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                if entry.method == method::GITHUB {
                    entry.method = crate::github_method::offline_method(
                        &self.roots.state_dir,
                        &entry,
                        manifest,
                    )
                    .to_owned();
                }
                entry
            })
            .collect::<Vec<_>>();
        let pkg = Some((pkg_mgr, env.is_android()));
        // Records are classified against the manifest as it is when each one
        // is read (as recovery does), not this earlier snapshot, so a commit
        // racing the report cannot look unclassifiable.
        let manifest_path = manifest::path(&self.roots.state_dir);
        // A partial entry list would make every pending install look
        // abandoned; `classify_pending` leaves those undetermined instead.
        let configured = self.config_complete.then_some(resolved.as_slice());
        match update_transition::classify_pending(configured, &manifest_path, self.roots, pkg) {
            Ok(records) => {
                for record in records {
                    self.push_pending(
                        record,
                        "an interrupted install-method change is pending; run 'shdeps update'",
                    );
                }
            }
            Err(error) => self.unreadable(
                None,
                &update_transition::transition_dir(&self.roots.state_dir),
                error,
            ),
        }

        #[cfg(unix)]
        {
            // The same command set publication recovery walks.
            let commands = resolved
                .iter()
                .map(|entry| entry.cmd.as_str())
                .chain(
                    manifest
                        .effective_entries()
                        .into_iter()
                        .map(|row| row.cmd.as_str()),
                )
                .filter(|cmd| config::valid_cmd_basename(cmd))
                .collect::<BTreeSet<_>>();
            for cmd in commands {
                let public = self.roots.bin_dir.join(cmd);
                if let Some(mut record) =
                    update_transition::classify_public_transition(&manifest_path, &public)
                {
                    // An unreadable record still belongs to whoever owns the
                    // command.
                    if record.name.is_none() {
                        record.name = resolved
                            .iter()
                            .map(|entry| (entry.name.as_str(), entry.cmd.as_str()))
                            .chain(
                                manifest
                                    .effective_entries()
                                    .into_iter()
                                    .map(|row| (row.name.as_str(), row.cmd.as_str())),
                            )
                            .find(|(_, owned)| *owned == cmd)
                            .map(|(name, _)| name.to_owned());
                    }
                    self.push_pending(
                        record,
                        "an interrupted command handoff is pending; run 'shdeps update'",
                    );
                }
            }

            // Every root a repo install, release install, or prune of either
            // may publish, including orphans and the old side of a handoff.
            let roots = resolved
                .iter()
                .filter(|entry| method::is_concrete_github(&entry.method))
                .map(|entry| entry.name.as_str())
                .chain(
                    manifest
                        .effective_entries()
                        .into_iter()
                        .filter(|row| method::is_concrete_github(&row.method))
                        .map(|row| row.name.as_str()),
                )
                .collect::<BTreeSet<_>>();
            for name in roots {
                self.check_checkout(name);
            }
        }
    }

    /// Reports one classified handoff record; `pending` is the warning for a
    /// record the next update finishes by itself.
    fn push_pending(&mut self, record: update_transition::PendingRecord, pending: &str) {
        let (kind, detail) = match record.verdict {
            PendingVerdict::Recoverable => (ProblemKind::RecoveryState, pending.to_owned()),
            PendingVerdict::Undetermined => (
                ProblemKind::RecoveryState,
                "an interrupted install-method change is pending, but whether the next update can retry it cannot be judged until the config reads; fix the unreadable config, then run 'shdeps update'".to_owned(),
            ),
            PendingVerdict::Blocked(reason) => (
                ProblemKind::BlockedTransition,
                format!(
                    "{}; 'shdeps update' fails until this is resolved",
                    without_record_path(&reason, &record.path)
                ),
            ),
        };
        self.push(kind, record.name.as_deref(), Some(&record.path), detail);
    }

    /// Reports the checkout journals and temp trees beside one managed root.
    ///
    /// Repo recovery fails closed on the checkout installer's transaction and
    /// on a Shdeps journal it cannot finish (a recorded collision, a
    /// malformed record, a foreign object at the root), so those block that
    /// package; any other journal is rolled forward or back by the next
    /// update. Both are in flight, not stuck, while the checkout lock has a
    /// live owner. Temp trees never block on their own, but nothing removes
    /// them, so they are reported for cleanup.
    #[cfg(unix)]
    fn check_checkout(&mut self, name: &str) {
        let logical = self
            .roots
            .install_dir
            .join(config::canonical_name(name, method::GITHUB_REPO));
        // The checkout lock physically resolves the root's parent, and each
        // journal records that exact spelling. Without a parent directory
        // there is nothing beside the root to inspect.
        let Some(root) = self.physical(&logical) else {
            return;
        };
        let state = match crate::repo_transition::pending_state(&root) {
            Ok(state) => state,
            Err(error) => {
                self.unreadable(Some(name), &root, error);
                return;
            }
        };
        // The installer keeps its transaction for its whole run, and Shdeps
        // moves a journaled checkout, under the shared checkout lock (not
        // Shdeps' state lock), so a live lock owner means work in progress.
        // Only probed when something is pending.
        let in_flight = (state.installer_transaction.is_some()
            || matches!(
                state.journal,
                Some(crate::repo_transition::JournalState::Blocked(..))
            ))
            && crate::checkout_lock::held_by_live_owner(&root);
        if let Some(transaction) = state.installer_transaction.as_ref().filter(|_| !in_flight) {
            self.push(
                ProblemKind::BlockedTransition,
                Some(name),
                Some(transaction),
                "checkout installer transaction is still present; if no checkout installer is running, rerun it before Shdeps; update and prune of this checkout refuse until then".to_owned(),
            );
        }
        match &state.journal {
            Some(crate::repo_transition::JournalState::Blocked(..)) if in_flight => {}
            Some(crate::repo_transition::JournalState::Blocked(journal, reason)) => self.push(
                ProblemKind::BlockedTransition,
                Some(name),
                Some(journal),
                format!(
                    "{} (checkout {}); 'shdeps update' and 'shdeps prune' of this checkout fail until it is resolved",
                    without_record_path(reason, journal),
                    root.display()
                ),
            ),
            Some(crate::repo_transition::JournalState::Pending(journal)) => self.push(
                ProblemKind::RecoveryState,
                Some(name),
                Some(journal),
                "an interrupted repository publication is pending; run 'shdeps update'".to_owned(),
            ),
            None => {}
        }
        // A journal may name a `<root>.tmp.<pid>` clone as the tree recovery
        // will publish; advising its deletion would lose the publication.
        if state.journal.is_some() {
            return;
        }
        for tree in temp_trees(&root) {
            self.push(
                ProblemKind::TempTree,
                Some(name),
                Some(&tree),
                "an interrupted install left this temporary tree, which shdeps never removes; delete it if no shdeps update is running".to_owned(),
            );
        }
    }

    /// Reports dangling tracked links and interrupted ledger reconciliations.
    fn check_ledgers(&mut self, name: &str) {
        for kind in [LinkKind::Bin, LinkKind::Extras] {
            let ledger = link_state::path(&self.roots.state_dir, name, kind);
            match link_state::read(&ledger) {
                Ok(paths) => {
                    for path in paths {
                        if !self.checked_links.contains(&path) && dangling(&path) {
                            self.push_dangling(kind, name, &path);
                        }
                    }
                }
                Err(error) => self.unreadable(Some(name), &ledger, error),
            }
            let journal = link_state::reconcile_path(&ledger);
            if exists(&journal) {
                self.push(
                    ProblemKind::RecoveryState,
                    Some(name),
                    Some(&journal),
                    "an interrupted link update is pending; run 'shdeps update'".to_owned(),
                );
            }
        }
    }

    fn check_deferrals(&mut self) {
        let state_dir = self.roots.state_dir.clone();
        for name in deferral_notice::recorded(&state_dir, deferral_notice::Kind::Posts) {
            self.push(
                ProblemKind::DeferredPost,
                Some(&name),
                Some(&deferral_notice::path(&state_dir, deferral_notice::Kind::Posts)),
                "post() hook needs sudo and was deferred without a terminal; run 'shdeps update' from a terminal".to_owned(),
            );
        }
        for name in deferral_notice::recorded(&state_dir, deferral_notice::Kind::Uninstalls) {
            self.push(
                ProblemKind::DeferredUninstall,
                Some(&name),
                Some(&deferral_notice::path(&state_dir, deferral_notice::Kind::Uninstalls)),
                "uninstall() hook needs sudo and was deferred without a terminal; run 'shdeps prune' from a terminal".to_owned(),
            );
        }
    }

    /// Reports records an interrupted update or prune leaves for recovery.
    fn check_recovery(&mut self) {
        let state_dir = self.roots.state_dir.clone();
        let deferred = deferral_notice::recorded(&state_dir, deferral_notice::Kind::Posts);
        // Prune keeps its journal for a deferred uninstall; that deferral row
        // already explains the leftover records, and its "run 'shdeps prune'"
        // also retries any other unfinished record in the same journal.
        let deferred_uninstalls =
            !deferral_notice::recorded(&state_dir, deferral_notice::Kind::Uninstalls).is_empty();
        match hooks::pending_posts(&state_dir) {
            Ok(names) => {
                // A deferred post is also pending; its deferral row already
                // explains it and carries the right remediation.
                for name in names.iter().filter(|name| !deferred.contains(*name)) {
                    self.push(
                        ProblemKind::PendingPost,
                        Some(name),
                        Some(&hooks::pending_post_path(&state_dir, name)),
                        "post() hook has not completed; run 'shdeps update'".to_owned(),
                    );
                }
            }
            Err(error) => self.unreadable(None, &hooks::pending_posts_dir(&state_dir), error),
        }

        // Method-transition records are classified one by one in
        // `check_transitions`.
        for (dir, detail) in [
            (
                crate::repo_transition::fresh_index_dir(&state_dir),
                "an interrupted first repo install is pending; run 'shdeps update'",
            ),
            (
                crate::prune_journal::dir_path(&state_dir),
                "prune left cleanup records (interrupted, or an uninstall() hook failed); run 'shdeps prune'",
            ),
        ] {
            if deferred_uninstalls && dir == crate::prune_journal::dir_path(&state_dir) {
                continue;
            }
            match non_empty_dir(&dir) {
                Ok(false) => {}
                Ok(true) => self.push(
                    ProblemKind::RecoveryState,
                    None,
                    Some(&dir),
                    detail.to_owned(),
                ),
                Err(error) => self.unreadable(None, &dir, error),
            }
        }
    }

    /// Reports repo and release sources stuck behind their remote. The
    /// update that fails to refresh them only warns once per run and
    /// forgets; this is the durable signal.
    fn check_stale_remotes(&mut self, entries: &[Entry], manifest: &Manifest) {
        let candidates = entries
            .iter()
            .filter(|entry| manifest.get(&entry.name).is_some())
            .filter_map(|entry| {
                let source = match dep_links::concrete_method(&entry.method, &entry.name, manifest)
                    .as_str()
                {
                    method::GITHUB_REPO => stale_remote::Source::Repo,
                    method::GITHUB_RELEASE => stale_remote::Source::Release,
                    _ => return None,
                };
                let root = self.roots.install_dir.join(&entry.name);
                // A repo root that is a symlink is a development clone.
                let subject = source == stale_remote::Source::Release
                    || !fs::symlink_metadata(&root).is_ok_and(|meta| meta.file_type().is_symlink());
                Some(stale_remote::Candidate {
                    name: entry.name.clone(),
                    source,
                    root,
                    subject,
                })
            })
            .collect::<Vec<_>>();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let state_dir = self.roots.state_dir.clone();
        for stale in stale_remote::find(&state_dir, &candidates, now, crate::cli::remote_ttl()) {
            self.push(
                ProblemKind::StaleRemote,
                Some(&stale.name),
                Some(&stale.root),
                stale.detail,
            );
        }
    }

    fn finish(mut self) -> Report {
        if state::recorded_lock_owner_alive(&self.roots.state_dir) {
            self.problems.retain(|problem| !problem.kind.transient());
        }
        self.problems.sort_by(|left, right| {
            (&left.package, left.kind, &left.path).cmp(&(&right.package, right.kind, &right.path))
        });
        Report {
            problems: self.problems,
        }
    }
}

/// A symlink whose final target is missing. Regular files and absent paths
/// are not dangling: Shdeps never replaces a file it did not create, and an
/// absent extra is recreated by the next update's relink.
fn dangling(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && fs::metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Mirrors `process::executable_path` on metadata the caller already holds.
fn executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Interrupted clones (`<root>.tmp.<pid>`) and release staging trees
/// (`.<root>.tmp.<pid>`) beside `root`. Atomic-write temps carry a nonce after
/// the pid and are not matched.
#[cfg(unix)]
fn temp_trees(root: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name().and_then(|n| n.to_str()))
    else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let clone = format!("{name}.tmp.");
    let staging = format!(".{name}.tmp.");
    let mut trees = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|file| {
                file.strip_prefix(&clone)
                    .or_else(|| file.strip_prefix(&staging))
                    .is_some_and(|pid| {
                        !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    trees.sort();
    trees
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn non_empty_dir(dir: &Path) -> io::Result<bool> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Drops a handoff record's own path from the reason recovery gives for
/// refusing it.
///
/// Recovery's error is the only place an update can name the record, so its
/// reasons embed the path (`...: <record>`, `... at <record> and retry`).
/// A health row already carries that path in its own column, and repeating
/// it made one row several hundred characters long. Only the path introduced
/// by `: ` or ` at ` and ending a phrase (at the end, or before `;`, `,`,
/// a space, or `)`) is removed with its connector; a longer path that merely
/// starts with it, and every other path a reason names, stay. That assumes no
/// reason names a different path spelled as the record's plus a space, and a
/// prefix occurrence that is not at a boundary is skipped, not rescanned: no
/// reason has either shape, and both fail safe by leaving the path in.
fn without_record_path(reason: &str, record: &Path) -> String {
    let shown = record.display().to_string();
    let mut reason = reason.to_owned();
    if shown.is_empty() {
        return reason;
    }
    for connector in [": ", " at "] {
        let needle = format!("{connector}{shown}");
        let mut from = 0;
        while let Some(found) = reason[from..].find(&needle).map(|at| from + at) {
            let end = found + needle.len();
            if reason[end..]
                .chars()
                .next()
                .is_none_or(|next| matches!(next, ';' | ',' | ' ' | ')'))
            {
                reason.replace_range(found..end, "");
                from = found;
            } else {
                from = end;
            }
        }
    }
    reason
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Problem, ProblemKind, Report, without_record_path, write_tsv};

    #[test]
    fn record_path_is_dropped_only_where_it_repeats_the_path_column() {
        let record = Path::new("/state/t/abc.json");
        for (reason, expected) in [
            (
                "unexpected entry in method transition state: /state/t/abc.json",
                "unexpected entry in method transition state",
            ),
            (
                "verify installed state, remove the stale transition record at /state/t/abc.json and retry",
                "verify installed state, remove the stale transition record and retry",
            ),
            ("bad: /state/t/abc.json; then", "bad; then"),
            ("bad (x: /state/t/abc.json)", "bad (x)"),
            // A longer path that starts with the record's stays whole.
            (
                "clone remains at /state/t/abc.json.tmp.1",
                "clone remains at /state/t/abc.json.tmp.1",
            ),
            (
                "child at /state/t/abc.json/x",
                "child at /state/t/abc.json/x",
            ),
            // Without a connector the path may be part of a command.
            ("run 'rm /state/t/abc.json'", "run 'rm /state/t/abc.json'"),
            (
                "malformed record: missing field",
                "malformed record: missing field",
            ),
        ] {
            assert_eq!(without_record_path(reason, record), expected, "{reason}");
        }
        assert_eq!(without_record_path("x: ", Path::new("")), "x: ");
    }

    #[test]
    fn rows_always_have_five_fields_whatever_the_values_contain() {
        // dot doctor splits on tabs and reads one row per line, so embedded
        // separators must never shift columns or split a row.
        let report = Report {
            problems: vec![
                Problem {
                    kind: ProblemKind::WrongTarget,
                    package: Some("own\ter/to\nol".to_owned()),
                    path: Some(PathBuf::from("/tmp/a\tb\r\nc")),
                    detail: "line one\nline\ttwo\r".to_owned(),
                },
                Problem {
                    kind: ProblemKind::RecoveryState,
                    package: None,
                    path: None,
                    detail: String::new(),
                },
                Problem {
                    kind: ProblemKind::DeferredPost,
                    package: Some(String::new()),
                    path: Some(PathBuf::new()),
                    detail: "hint".to_owned(),
                },
            ],
        };
        let mut out = Vec::new();

        write_tsv(&report, &mut out).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "warn\town er/to ol\twrong-target\t/tmp/a b  c\tline one line two \n\
             warn\t-\trecovery-state\t-\t-\n\
             warn\t-\tdeferred-post\t-\thint\n"
        );
        assert!(out.lines().all(|row| row.split('\t').count() == 5));
    }
}
