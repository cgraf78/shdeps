//! Hidden bridge commands for Bash wrappers and hook preludes.
//!
//! `shdeps __api` is not a user-facing CLI, but it is still compatibility
//! surface. The Bash wrapper will call these commands from command substitution
//! and hooks, so stdout must stay machine-clean and each command must be honest
//! about whether it is cheap. This initial registry intentionally includes only
//! read-only commands and path helpers; mutating bridge calls land with the Rust
//! modules that own those side effects.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Result;
use crate::config;
use crate::dep_path;
use crate::errors::Error;
use crate::extras;
use crate::http::Curl;
use crate::link_state::{self, Kind};
use crate::pkg::{self, CommandSpec};
use crate::platform;
use crate::process::{self, Process, Runner};
use crate::runtime::{self, Overrides, ProcessEnv};
use crate::update::Options;
use crate::update_release::{self, ReleaseRequest};

/// Runs one hidden bridge command.
pub fn run<W, E>(
    args: &[String],
    overrides: &Overrides,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let Some(command) = args.first().map(String::as_str) else {
        writeln!(stderr, "error: __api requires a command")?;
        return Ok(2);
    };
    let rest = &args[1..];
    let roots = runtime::roots(&ProcessEnv, overrides);
    let env = runtime::runtime_env(&ProcessEnv);

    match command {
        "version" => {
            // This is wrapper-binary ABI, not the shdeps git commit version.
            // Keep it tiny and config-free so old wrappers can negotiate with
            // new binaries even on partially bootstrapped machines.
            writeln!(stdout, "abi:1")?;
            Ok(0)
        }
        "env-snapshot" => {
            writeln!(stdout, "install_dir={}", roots.install_dir.display())?;
            writeln!(stdout, "bin_dir={}", roots.bin_dir.display())?;
            writeln!(stdout, "git_dev_dir={}", roots.git_dev_dir.display())?;
            writeln!(stdout, "platform={}", env.platform())?;
            writeln!(stdout, "pkg_mgr={}", runtime::pkg_mgr(&ProcessEnv))?;
            writeln!(
                stdout,
                "force={}",
                flag(runtime::force(&ProcessEnv, overrides))
            )?;
            writeln!(
                stdout,
                "reinstall={}",
                flag(runtime::reinstall(&ProcessEnv, overrides))
            )?;
            writeln!(stdout, "abi=1")?;
            Ok(0)
        }
        "platform-match" => predicate(rest.first(), stderr, |spec| {
            platform::platform_match(spec, &env)
        }),
        "host-match" => predicate(rest.first(), stderr, |spec| {
            platform::host_match(spec, &env)
        }),
        "filter-match" => {
            let Some(spec) = rest.first() else {
                writeln!(stderr, "error: __api filter-match requires a spec")?;
                return Ok(2);
            };
            Ok(platform::filter_match(spec, &env).exit_code())
        }
        "platform" => {
            writeln!(stdout, "{}", env.platform())?;
            Ok(0)
        }
        "force" => Ok(if runtime::force(&ProcessEnv, overrides) {
            0
        } else {
            1
        }),
        "reinstall" => Ok(if runtime::reinstall(&ProcessEnv, overrides) {
            0
        } else {
            1
        }),
        "load-count" => {
            writeln!(stdout, "{}", load_count(&roots.conf_dir)?)?;
            Ok(0)
        }
        "pkg-mgr" => {
            writeln!(stdout, "{}", runtime::pkg_mgr(&ProcessEnv))?;
            Ok(0)
        }
        "install-dir" => {
            writeln!(stdout, "{}", roots.install_dir.display())?;
            Ok(0)
        }
        "git-dev-dir" => {
            writeln!(stdout, "{}", roots.git_dev_dir.display())?;
            Ok(0)
        }
        "bin-dir" => {
            writeln!(stdout, "{}", roots.bin_dir.display())?;
            Ok(0)
        }
        "dep-root" => {
            let Some(target) = rest.first() else {
                writeln!(stderr, "error: __api dep-root requires a dependency name")?;
                return Ok(2);
            };
            dep_root(target, overrides, stdout)
        }
        "dep-path" => {
            let (Some(target), Some(rel)) = (rest.first(), rest.get(1)) else {
                writeln!(
                    stderr,
                    "error: __api dep-path requires a dependency name and relative path"
                )?;
                return Ok(2);
            };
            dep_path(target, rel, overrides, stdout)
        }
        "dep-file" => {
            let (Some(target), Some(rel)) = (rest.first(), rest.get(1)) else {
                writeln!(
                    stderr,
                    "error: __api dep-file requires a dependency name and relative path"
                )?;
                return Ok(2);
            };
            dep_file(target, rel, overrides, stdout)
        }
        "link-extras" => {
            let (Some(name), Some(install_dir)) = (rest.first(), rest.get(1)) else {
                writeln!(
                    stderr,
                    "error: __api link-extras requires a dependency name and install dir"
                )?;
                return Ok(2);
            };
            // Keep the bridge as a thin adapter over the Rust owner. The Bash
            // wrapper should not rediscover man/completion conventions because
            // stale-link cleanup depends on one shared `.links` state format.
            extras::link(
                &roots.state_dir,
                &roots.install_dir,
                name,
                Path::new(install_dir),
            )?;
            Ok(0)
        }
        "unlink-extras" => {
            let Some(name) = rest.first() else {
                writeln!(
                    stderr,
                    "error: __api unlink-extras requires a dependency name"
                )?;
                return Ok(2);
            };
            link_state::unlink_tracked(&link_state::path(&roots.state_dir, name, Kind::Extras))?;
            Ok(0)
        }
        "pkg-install" => {
            let Some(package) = rest.first() else {
                writeln!(stderr, "error: __api pkg-install requires a package name")?;
                return Ok(2);
            };
            pkg_install(package, stderr)
        }
        "pkg-install-for-mgr" => {
            if rest.is_empty() {
                writeln!(stderr, "error: __api pkg-install-for-mgr requires specs")?;
                return Ok(2);
            };
            pkg_install_for_mgr(rest, stderr)
        }
        "require-sudo" => require_sudo(),
        "github-release-install" => {
            let (Some(name), Some(cmd)) = (rest.first(), rest.get(1)) else {
                writeln!(
                    stderr,
                    "error: __api github-release-install requires a name and command"
                )?;
                return Ok(2);
            };
            github_release_install(
                name,
                cmd,
                rest.get(2).map(String::as_str),
                rest.get(3).map(Path::new),
                overrides,
                stdout,
                stderr,
            )
        }
        other => {
            writeln!(stderr, "error: unknown __api command '{other}'")?;
            Ok(2)
        }
    }
}

fn github_release_install<W, E>(
    name: &str,
    cmd: &str,
    repo: Option<&str>,
    bin_path: Option<&Path>,
    overrides: &Overrides,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32>
where
    W: Write,
    E: Write,
{
    let roots = runtime::roots(&ProcessEnv, overrides);
    let runtime_env = runtime::runtime_env(&ProcessEnv);
    let name = config::canonical_name(name, "github:release");
    let repo = config::canonical_name(repo.unwrap_or(&name), "github:release");
    // Apply the same basename validator the config-side `parse_entry`
    // path uses (see `config::valid_cmd_basename`). Without this guard
    // the hidden API would be a second, looser entry point: a hook —
    // or any caller of the Bash `shdeps_github_release_install`
    // wrapper — could pass `cmd=/etc/passwd` and, when `bin_path` is
    // omitted, the default `roots.bin_dir.join(cmd)` would resolve
    // to the absolute path verbatim under Rust's `Path::join`
    // semantics. The downstream release-install pipeline then
    // renames the staged executable onto that exact path. The
    // config-side hardening would be a half-measure if the bridge
    // bypassed the same validator. `bin_path` stays unvalidated
    // here because the bridge intentionally exposes it as a
    // hook-author flexibility point (custom install layouts under
    // managed roots that the default `<bin_dir>/<cmd>` does not
    // cover); hooks are trusted user code per `hooks.rs`'s file
    // comment, so the bin_path-trust contract is consistent with
    // the rest of the hook surface.
    if !config::valid_dep_name(&name)
        || !config::valid_dep_name(&repo)
        || !config::valid_cmd_basename(cmd)
    {
        writeln!(stderr, "error: invalid github-release-install arguments")?;
        return Ok(2);
    }

    let public_bin = bin_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| roots.bin_dir.join(cmd));
    let default_options = Options::default();
    let options = Options {
        // The bridge is used by hook code as an API function, so it must obey
        // the same force/reinstall knobs as a top-level update. Otherwise a
        // custom hook could see different freshness behavior depending on
        // whether the caller used Bash shdeps or the Rust prelude.
        force: runtime::force(&ProcessEnv, overrides),
        reinstall: runtime::reinstall(&ProcessEnv, overrides),
        verbose: false,
        now: default_options.now,
        remote_ttl: default_options.remote_ttl,
    };
    let env_vars = std::env::vars().collect();
    let request = ReleaseRequest {
        name: &name,
        cmd,
        repo: &repo,
        public_bin: &public_bin,
    };
    // Hidden API calls install exactly one release dependency, so there is no
    // batch to prefetch. Keep the call on the shared installer path with an
    // empty prefetch object so token/freshness behavior stays identical to the
    // top-level update flow.
    let prefetch = update_release::Prefetch::default();
    let request_context = update_release::RequestContext {
        roots: &roots,
        runtime_env: &runtime_env,
        env_vars: &env_vars,
        runner: &Process,
        client: &Curl,
        options,
        prefetch: &prefetch,
    };
    let outcome = update_release::install_request(&request, &request_context)?;

    if outcome.failed {
        writeln!(
            stderr,
            "warning: {name} github release install failed: {}",
            outcome.detail
        )?;
        return Ok(1);
    }
    if outcome.changed {
        mark_changed(&roots.state_dir, &name)?;
        writeln!(stdout, "  {name} installed -- {}", outcome.detail)?;
    } else {
        writeln!(stdout, "  {name} -- {}", outcome.detail)?;
    }
    Ok(0)
}

fn mark_changed(state_dir: &Path, name: &str) -> Result<()> {
    let Some(txn_id) = std::env::var("SHDEPS_UPDATE_TXN_ID")
        .ok()
        .filter(|value| valid_marker_component(value))
    else {
        return Ok(());
    };
    if !config::valid_dep_name(name) {
        return Ok(());
    }

    let marker = state_dir.join(".changed-markers").join(txn_id).join(name);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Hooks run in subprocesses, so changed-state has to cross the process
    // boundary through a tiny durable marker. Using the dependency name as a
    // relative path preserves owner/repo grouping while `valid_dep_name` above
    // prevents path traversal from a malicious hook argument.
    std::fs::write(marker, b"")?;
    Ok(())
}

fn valid_marker_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value != "."
        && value != ".."
        && !value.chars().any(char::is_whitespace)
}

fn dep_root<W>(target: &str, overrides: &Overrides, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, overrides);
    path_result(
        dep_path::root(
            target,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn dep_path<W>(target: &str, rel: &str, overrides: &Overrides, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, overrides);
    path_result(
        dep_path::path(
            target,
            rel,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn dep_file<W>(target: &str, rel: &str, overrides: &Overrides, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    let roots = runtime::roots(&ProcessEnv, overrides);
    path_result(
        dep_path::file(
            target,
            rel,
            &roots.dep_path_roots(),
            &runtime::runtime_env(&ProcessEnv),
        ),
        stdout,
    )
}

fn pkg_install_for_mgr<E>(specs: &[String], stderr: &mut E) -> Result<i32>
where
    E: Write,
{
    let pkg_mgr = package_manager();
    if pkg_mgr.is_empty() {
        writeln!(
            stderr,
            "warning: no package manager found - cannot install package"
        )?;
        return Ok(1);
    }

    for spec in specs {
        let Some((mgr, package)) = spec.split_once(':') else {
            continue;
        };
        if mgr == pkg_mgr {
            return pkg_install_with_mgr(&pkg_mgr, package, stderr);
        }
    }
    Ok(1)
}

fn pkg_install<E>(package: &str, stderr: &mut E) -> Result<i32>
where
    E: Write,
{
    let pkg_mgr = package_manager();
    if pkg_mgr.is_empty() {
        writeln!(
            stderr,
            "warning: no package manager found - cannot install {package}"
        )?;
        return Ok(1);
    }

    pkg_install_with_mgr(&pkg_mgr, package, stderr)
}

fn pkg_install_with_mgr<E>(pkg_mgr: &str, package: &str, stderr: &mut E) -> Result<i32>
where
    E: Write,
{
    if package.is_empty() {
        return Ok(1);
    }

    if let Some(refresh) = pkg::refresh(pkg_mgr) {
        // Bash treats metadata refresh as best effort: a stale repo cache
        // should not prevent an explicit hook fallback from checking whether
        // the requested package is available. Preserve that behavior here so
        // transient mirror failures do not block custom hooks unnecessarily.
        let _ = run_pkg_command(&refresh);
    }

    let Some(available) = pkg::available(pkg_mgr, package) else {
        writeln!(
            stderr,
            "warning: unsupported package manager {pkg_mgr} - cannot install {package}"
        )?;
        return Ok(1);
    };
    let output = run_pkg_command(&available)?;
    if !pkg::available_ok(pkg_mgr, output.success, &output.stdout) {
        writeln!(
            stderr,
            "warning: {package} not available in {pkg_mgr} repos"
        )?;
        return Ok(1);
    }

    let packages = [package.to_owned()];
    let Some(install) = pkg::install(pkg_mgr, &packages) else {
        return Ok(1);
    };
    if run_pkg_command(&install)?.success {
        Ok(0)
    } else {
        writeln!(stderr, "warning: failed to install {package}")?;
        Ok(1)
    }
}

fn package_manager() -> String {
    let cached = runtime::pkg_mgr(&ProcessEnv);
    if cached.is_empty() {
        process::detect_package_manager(&Process)
    } else {
        cached
    }
}

fn run_pkg_command(command: &CommandSpec) -> Result<crate::process::Output> {
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(Process.run(&command.program, &args, None)?)
}

fn require_sudo() -> Result<i32> {
    let uid = Process.run("id", &["-u"], Some(Duration::from_secs(2)))?;
    if uid.success && uid.stdout.trim() == "0" {
        return Ok(0);
    }

    let non_interactive = Process.run("sudo", &["-n", "true"], Some(Duration::from_secs(2)))?;
    if non_interactive.success {
        return Ok(0);
    }

    if std::env::var("SHDEPS_QUIET").as_deref() == Ok("1") {
        return Ok(1);
    }

    // Match the Bash helper's escalation order: only the final attempt may
    // prompt. Hooks call this before choosing a fallback installer, so quiet
    // mode must never block on an unexpected sudo password prompt.
    Ok(if Process.run("sudo", &["true"], None)?.success {
        0
    } else {
        1
    })
}

fn path_result<W>(result: Result<PathBuf>, stdout: &mut W) -> Result<i32>
where
    W: Write,
{
    match result {
        Ok(path) => {
            writeln!(stdout, "{}", path.display())?;
            Ok(0)
        }
        Err(Error::Resolve(error)) => Ok(error.exit_code()),
        Err(error) => Err(error),
    }
}

fn predicate<E>(
    spec: Option<&String>,
    stderr: &mut E,
    matches: impl FnOnce(&str) -> bool,
) -> Result<i32>
where
    E: Write,
{
    let Some(spec) = spec else {
        writeln!(stderr, "error: __api predicate requires a spec")?;
        return Ok(2);
    };
    Ok(if matches(spec) { 0 } else { 1 })
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn load_count(conf_dir: &Path) -> Result<usize> {
    Ok(config::load_dir(conf_dir)?.len())
}
