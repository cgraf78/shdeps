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

use crate::config;
use crate::dep_path;
use crate::errors::Error;
use crate::extras;
use crate::link_state::{self, Kind};
use crate::pkg::{self, CommandSpec};
use crate::platform;
use crate::process::{self, Process, Runner};
use crate::runtime::{self, Overrides, ProcessEnv};
use crate::Result;

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
        "require-sudo" | "github-release-install" => {
            // These names are part of the wrapper ABI, so recognize them now
            // instead of letting callers see "unknown command" and infer that
            // the bridge surface changed. They remain explicit runtime
            // failures until the package, extras-linking, and GitHub release
            // owners land with their real side-effect implementations.
            writeln!(stderr, "error: __api {command} is not implemented yet")?;
            Ok(1)
        }
        other => {
            writeln!(stderr, "error: unknown __api command '{other}'")?;
            Ok(2)
        }
    }
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
    if value {
        "1"
    } else {
        "0"
    }
}

fn load_count(conf_dir: &Path) -> Result<usize> {
    Ok(config::load_dir(conf_dir)?.len())
}
