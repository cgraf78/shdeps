//! Shared plumbing for dependency install hooks.
//!
//! Custom hooks that fall back to a manual install keep re-implementing the same
//! three idioms: marking a dependency skipped (with a reason), locating a
//! language runtime, and writing a launcher that `exec`s an interpreter against
//! a payload. These helpers own that plumbing in Rust so the public Bash
//! functions (`shdeps_skip`/`shdeps_skipped`/`shdeps_skip_reason`/`shdeps_unskip`,
//! `shdeps_find_runtime`, `shdeps_write_wrapper`) stay thin one-line shims and
//! every hook shares one implementation.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config;
use crate::process::Runner;

/// Reason recorded when a hook marks a dependency skipped without giving one.
const DEFAULT_SKIP_REASON: &str = "skipped";

/// Filename of the per-dependency skip marker, under its install directory.
const SKIP_MARKER: &str = ".skipped";

/// Timeout for the `--version` probe used by [`find_runtime`].
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves a dependency's skip-marker path, or `None` for an unsafe name.
///
/// The dependency name becomes a path component under the install directory, so
/// it is validated with the same guard config parsing uses to keep a hook from
/// escaping the managed root.
fn skip_marker_path(install_dir: &Path, dep: &str) -> Option<PathBuf> {
    config::valid_dep_name(dep).then(|| install_dir.join(dep).join(SKIP_MARKER))
}

/// Records a skip marker for `dep` containing `reason`.
///
/// Returns `false` without writing anything when `dep` is not a valid
/// dependency name. An empty reason is stored as a generic default.
pub fn skip_mark(install_dir: &Path, dep: &str, reason: &str) -> io::Result<bool> {
    let Some(marker) = skip_marker_path(install_dir, dep) else {
        return Ok(false);
    };
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let reason = if reason.is_empty() {
        DEFAULT_SKIP_REASON
    } else {
        reason
    };
    std::fs::write(marker, format!("{reason}\n"))?;
    Ok(true)
}

/// Reports whether `dep` currently has a skip marker.
#[must_use]
pub fn skip_check(install_dir: &Path, dep: &str) -> bool {
    skip_marker_path(install_dir, dep).is_some_and(|marker| marker.exists())
}

/// Returns the first line of `dep`'s skip-marker reason, if it is marked.
#[must_use]
pub fn skip_reason(install_dir: &Path, dep: &str) -> Option<String> {
    let marker = skip_marker_path(install_dir, dep)?;
    let content = std::fs::read_to_string(marker).ok()?;
    Some(content.lines().next().unwrap_or("").to_owned())
}

/// Removes `dep`'s skip marker. A missing marker (or invalid name) is a no-op.
pub fn skip_clear(install_dir: &Path, dep: &str) -> io::Result<()> {
    let Some(marker) = skip_marker_path(install_dir, dep) else {
        return Ok(());
    };
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// A request to locate a usable language runtime.
pub struct RuntimeQuery<'a> {
    /// Candidate binary names, tried in order (e.g. `openjdk`, `java`).
    pub names: &'a [String],
    /// Extra directories searched before `$PATH` (e.g. Homebrew opt prefixes).
    pub dirs: &'a [String],
    /// Reject a candidate whose `--version` output contains this substring.
    pub reject: Option<&'a str>,
    /// Require a successful `--version` probe before accepting a candidate.
    pub verify: bool,
}

/// Finds the first usable runtime for `query`, or `None` when none qualifies.
///
/// Each name is checked against `dirs` (as `<dir>/<name>`) and then `$PATH`. The
/// first executable file that survives the optional `reject`/`verify` probe is
/// returned. `reject`/`verify` run `<candidate> --version` once; when neither is
/// set no probe runs, so plain discovery stays a cheap filesystem check.
#[must_use]
pub fn find_runtime(query: &RuntimeQuery, runner: &impl Runner) -> Option<PathBuf> {
    for name in query.names {
        for dir in query.dirs {
            let candidate = Path::new(dir).join(name);
            if is_executable_file(&candidate) && accept(&candidate, query, runner) {
                return Some(candidate);
            }
        }
        if let Some(candidate) = search_path(name) {
            if accept(&candidate, query, runner) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Specification for the launcher [`write_wrapper`] generates.
pub struct WrapperSpec<'a> {
    /// Wrapper basename created under the bin directory.
    pub name: &'a str,
    /// Interpreter the wrapper execs (e.g. `java`, `php`).
    pub interpreter: &'a str,
    /// Interpreter arguments placed before the payload (e.g. `-jar`).
    pub interp_args: &'a [String],
    /// Payload the interpreter runs (e.g. a `.jar`/`.phar` path).
    pub payload: &'a str,
    /// `export VAR=value` lines emitted before the exec (e.g. a gem `PATH`).
    pub env: &'a [String],
}

/// Writes an executable launcher to `<bin_dir>/<name>` and returns its path.
///
/// The wrapper execs `<interpreter> <interp_args...> <payload> "$@"`, so hooks
/// share one shell-safe generator instead of hand-writing the shebang and
/// `exec` line. Interpreter, args, and payload are single-quoted; `env` lines
/// are emitted verbatim so a value like `PATH=...:$PATH` still expands at run
/// time. Returns `None` without writing when `name` is not a safe basename.
pub fn write_wrapper(bin_dir: &Path, spec: &WrapperSpec) -> io::Result<Option<PathBuf>> {
    if !config::valid_cmd_basename(spec.name) {
        return Ok(None);
    }
    std::fs::create_dir_all(bin_dir)?;
    let wrapper = bin_dir.join(spec.name);

    let mut script = String::from("#!/usr/bin/env bash\n");
    for line in spec.env {
        script.push_str("export ");
        script.push_str(line);
        script.push('\n');
    }
    script.push_str("exec ");
    script.push_str(&sh_quote(spec.interpreter));
    for arg in spec.interp_args {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push(' ');
    script.push_str(&sh_quote(spec.payload));
    script.push_str(" \"$@\"\n");

    std::fs::write(&wrapper, script)?;
    let mut perms = std::fs::metadata(&wrapper)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper, perms)?;
    Ok(Some(wrapper))
}

/// Reports whether `path` is a regular file with any execute bit set.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Resolves `name` to an executable: a path as-is, otherwise the first `$PATH`
/// directory that holds an executable file of that name.
fn search_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable_file(&path).then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Applies the optional `--version` reject/verify probe to a candidate.
///
/// A probe that fails to spawn is accepted unless `verify` demanded success, so
/// a runtime whose `--version` is merely unusual is not silently rejected.
fn accept(candidate: &Path, query: &RuntimeQuery, runner: &impl Runner) -> bool {
    if query.reject.is_none() && !query.verify {
        return true;
    }
    let Ok(output) = runner.run(
        &candidate.to_string_lossy(),
        &["--version"],
        Some(PROBE_TIMEOUT),
    ) else {
        return !query.verify;
    };
    if query.verify && !output.success {
        return false;
    }
    match query.reject {
        Some(reject) => !format!("{}{}", output.stdout, output.stderr).contains(reject),
        None => true,
    }
}

/// Single-quotes a value so it is one literal argument in the generated wrapper.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
