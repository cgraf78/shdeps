//! Generated Bash prelude used by Rust hook subprocesses.
//!
//! Hook files are still Bash, but their `shdeps_*` helpers should bridge into
//! the Rust binary instead of sourcing the public wrapper for every hook
//! subprocess. The prelude keeps that boundary explicit: most helpers are
//! one-line bridge calls into `shdeps __api`, while the few helpers that must
//! act in the current shell stay local and deliberately tiny.

/// Returns the Bash source text for the Rust hook compatibility prelude.
#[must_use]
pub fn source() -> &'static str {
    SOURCE
}

const SOURCE: &str = r#"# shdeps Rust hook prelude.
# This file is sourced into a short-lived Bash hook subprocess. Keep bridge
# helpers small: hook authors expect normal shell functions, but Rust must stay
# the single owner of install state, path resolution, and extras/link cleanup.

shdeps_version() { command shdeps version "$@"; }
shdeps_update() { command shdeps update "$@"; }
shdeps_self_update() { command shdeps self-update "$@"; }
shdeps_load() { command shdeps __api load-count "$@"; }
shdeps_prune() { command shdeps prune "$@"; }

shdeps_platform_match() { command shdeps __api platform-match "$@"; }
shdeps_host_match() { command shdeps __api host-match "$@"; }
shdeps_filter_match() { command shdeps __api filter-match "$@"; }

# Snapshot answers are parent-resolved and exported into every hook
# subprocess, so read them from the environment instead of spawning a
# recursive `shdeps __api` call per query (dozens per update). Each cached
# form is exactly what the bridge would print: the parent exports its own
# resolved roots, and the flag queries match the bridge's env-only semantics
# (a nested `__api` call carries no CLI overrides). Unset-or-empty falls
# back to the bridge so probes without an export, or hooks that unset the
# variables, keep working; `pkg-mgr` is the exception (`-`, not `:-`)
# because the bridge's only input is that same variable and empty already
# means "none detected", so even an empty export is a definitive answer.
shdeps_platform() { printf '%s\n' "${SHDEPS_HOOK_PLATFORM:-$(command shdeps __api platform "$@")}"; }
shdeps_force() { [[ "${SHDEPS_FORCE:-0}" == "1" ]]; }
shdeps_reinstall() { [[ "${SHDEPS_REINSTALL:-0}" == "1" ]]; }
shdeps_pkg_mgr() { printf '%s\n' "${SHDEPS_PKG_MGR-$(command shdeps __api pkg-mgr "$@")}"; }
shdeps_pkg_install() { command shdeps __api pkg-install "$@"; }
shdeps_pkg_install_for_mgr() { command shdeps __api pkg-install-for-mgr "$@"; }
# Exit 75 is the private parent-prompt request returned only for this hook.
shdeps_require_sudo() {
  local _shdeps_sudo_status
  command shdeps __api require-sudo "$@"
  _shdeps_sudo_status=$?
  if [[ "$_shdeps_sudo_status" -eq 75 && -n "${SHDEPS_HOOK_SUDO_REQUEST:-}" ]]; then
    exit "$_shdeps_sudo_status"
  fi
  return "$_shdeps_sudo_status"
}
shdeps_install_dir() { printf '%s\n' "${SHDEPS_INSTALL_DIR:-$(command shdeps __api install-dir "$@")}"; }
shdeps_git_dev_dir() { printf '%s\n' "${SHDEPS_GIT_DEV_DIR:-$(command shdeps __api git-dev-dir "$@")}"; }
shdeps_bin_dir() { printf '%s\n' "${SHDEPS_BIN_DIR:-$(command shdeps __api bin-dir "$@")}"; }

shdeps_dep_root() { command shdeps __api dep-root "$@"; }
shdeps_dep_path() { command shdeps __api dep-path "$@"; }
shdeps_dep_file() { command shdeps __api dep-file "$@"; }
shdeps_dep_links() { command shdeps __api dep-links "$@"; }
shdeps_dep_source() {
  local _shdeps_source_path
  _shdeps_source_path=$(command shdeps __api dep-file "$@") || return $?
  . "$_shdeps_source_path"
}

shdeps_link_extras() { command shdeps __api link-extras "$@"; }
shdeps_unlink_extras() { command shdeps __api unlink-extras "$@"; }

shdeps_github_release_install() { command shdeps __api github-release-install "$@"; }

shdeps_skip() { command shdeps __api skip-mark "$@"; }
shdeps_skipped() { command shdeps __api skip-check "$@"; }
shdeps_skip_reason() { command shdeps __api skip-reason "$@"; }
shdeps_unskip() { command shdeps __api skip-clear "$@"; }
shdeps_find_runtime() { command shdeps __api find-runtime "$@"; }
shdeps_write_wrapper() { command shdeps __api write-wrapper "$@"; }

# Download helper carrying the same stall guards as shdeps' own transport.
#
# Hooks reach the network with bare `curl`, which waits forever for a body that
# never arrives; a wedged release download then hangs the whole update with no
# output. Owning the policy here keeps every hook consistent with the engine
# instead of each one restating (or forgetting) its own timeouts.
#
# `--speed-limit`/`--speed-time` rather than a fixed `--max-time`, so a large
# but healthy asset is not killed for being slow.
shdeps_curl() {
  curl --connect-timeout 10 --speed-limit 1024 --speed-time 60 --retry 3 "$@"
}

shdeps_log() { printf '%s\n' "$*"; }
# Prefix explicitly safe hook-authored warnings so the Rust parent can retain
# phase context on failure without forwarding arbitrary child stderr.
shdeps_warn() { printf 'shdeps-hook-warning: %s\n' "$*" >&2; }
shdeps_log_warn() { shdeps_warn "$@"; }
shdeps_log_ok() { shdeps_log "$@"; }
shdeps_log_dim() { shdeps_log "$@"; }
shdeps_log_header() { shdeps_log "$@"; }

shdeps_mark_changed() {
  local _shdeps_name="${1:-}"
  if [[ -z "${SHDEPS_UPDATE_TXN_ID:-}" || -z "${SHDEPS_STATE_DIR:-}" || -z "$_shdeps_name" ]]; then
    return 0
  fi
  case "$_shdeps_name" in
    /*|*' '*|*'	'*|*'|'*) return 1 ;;
  esac
  case "/$_shdeps_name/" in
    */../*) return 1 ;;
  esac
  local _shdeps_marker="$SHDEPS_STATE_DIR/.changed-markers/$SHDEPS_UPDATE_TXN_ID/$_shdeps_name"
  mkdir -p "$(dirname "$_shdeps_marker")" || return 1
  : >"$_shdeps_marker"
}
"#;

#[cfg(test)]
mod tests {
    use super::source;

    /// Runs `driver_bash` with the prelude sourced and a stub `shdeps` on
    /// PATH. Returns `(combined_output, stub_call_log)`.
    #[cfg(unix)]
    fn run_prelude_driver(driver_bash: &str, extra_env: &[(&str, &str)]) -> (String, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_support::temp_dir("shdeps-prelude-driver");
        let stub = dir.join("shdeps");
        std::fs::write(
            &stub,
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >>"$SHDEPS_STUB_LOG"
case "$1 $2" in
  "__api platform") printf 'stub-platform\n' ;;
  "__api force") exit 0 ;;
  "__api reinstall") exit 1 ;;
  "__api pkg-mgr") printf 'stub-pkg\n' ;;
  "__api install-dir") printf '/stub/install\n' ;;
  "__api git-dev-dir") printf '/stub/gitdev\n' ;;
  "__api bin-dir") printf '/stub/bin\n' ;;
  "__api skip-check") exit 3 ;;
  "__api skip-reason") printf 'stub-reason\n' ;;
  "__api platform-match") exit 4 ;;
  "__api dep-root") printf 'stub-dep-root\n' ;;
  "__api load-count") printf '42\n' ;;
  *) printf 'stub-unexpected: %s\n' "$*" >&2; exit 99 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.join("calls.log");
        let driver = dir.join("driver.sh");
        std::fs::write(
            &driver,
            format!(
                "{}\n{}\n",
                source().trim_end(),
                driver_bash.trim_start_matches('\n')
            ),
        )
        .unwrap();
        let mut command = std::process::Command::new("bash");
        command
            .arg(&driver)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env("SHDEPS_STUB_LOG", &log);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let output = command.output().expect("bash must run the prelude driver");
        assert_eq!(
            output.status.code(),
            Some(0),
            "driver failed: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        (String::from_utf8_lossy(&output.stdout).into_owned(), calls)
    }

    /// Non-cacheable helpers must always bridge into `shdeps __api` with
    /// exact argument passthrough. This pins the delegation contract for
    /// every helper the env-cache change does not touch.
    #[cfg(unix)]
    #[test]
    fn prelude_non_cached_helpers_always_delegate() {
        let (output, calls) = run_prelude_driver(
            r#"
shdeps_skipped tool; printf 'skip-check=%s\n' "$?"
printf 'skip-reason=%s\n' "$(shdeps_skip_reason tool)"
shdeps_platform_match linux; printf 'platform-match=%s\n' "$?"
printf 'dep-root=%s\n' "$(shdeps_dep_root tool)"
printf 'load=%s\n' "$(shdeps_load)"
"#,
            &[],
        );

        assert!(output.contains("skip-check=3\n"), "{output}");
        assert!(output.contains("skip-reason=stub-reason\n"), "{output}");
        assert!(output.contains("platform-match=4\n"), "{output}");
        assert!(output.contains("dep-root=stub-dep-root\n"), "{output}");
        assert!(output.contains("load=42\n"), "{output}");
        for expected in [
            "__api skip-check tool",
            "__api skip-reason tool",
            "__api platform-match linux",
            "__api dep-root tool",
            "__api load-count",
        ] {
            assert!(
                calls.lines().any(|line| line == expected),
                "missing bridge call {expected:?} in:\n{calls}"
            );
        }
    }

    /// Snapshot queries answer from the parent-exported environment
    /// without spawning. The stub records every `shdeps` invocation, so an
    /// empty call log proves the helpers cost zero subprocesses; the
    /// cli-level equivalence test proves the answers match the bridge.
    #[cfg(unix)]
    #[test]
    fn prelude_snapshot_queries_answer_from_exported_env() {
        let (output, calls) = run_prelude_driver(
            r#"
printf 'platform=%s\n' "$(shdeps_platform)"
shdeps_force; printf 'force=%s\n' "$?"
shdeps_reinstall; printf 'reinstall=%s\n' "$?"
printf 'pkg-mgr=%s\n' "$(shdeps_pkg_mgr)"
printf 'install-dir=%s\n' "$(shdeps_install_dir)"
printf 'git-dev-dir=%s\n' "$(shdeps_git_dev_dir)"
printf 'bin-dir=%s\n' "$(shdeps_bin_dir)"
printf 'args=%s\n' "$(shdeps_platform ignored-arg)"
"#,
            &[
                ("SHDEPS_HOOK_PLATFORM", "env-platform"),
                ("SHDEPS_FORCE", "1"),
                ("SHDEPS_REINSTALL", "0"),
                ("SHDEPS_PKG_MGR", "env-pkg"),
                ("SHDEPS_INSTALL_DIR", "/env/install"),
                ("SHDEPS_GIT_DEV_DIR", "/env/gitdev"),
                ("SHDEPS_BIN_DIR", "/env/bin"),
            ],
        );

        assert!(output.contains("platform=env-platform\n"), "{output}");
        assert!(output.contains("force=0\n"), "{output}");
        assert!(output.contains("reinstall=1\n"), "{output}");
        assert!(output.contains("pkg-mgr=env-pkg\n"), "{output}");
        assert!(output.contains("install-dir=/env/install\n"), "{output}");
        assert!(output.contains("git-dev-dir=/env/gitdev\n"), "{output}");
        assert!(output.contains("bin-dir=/env/bin\n"), "{output}");
        assert!(output.contains("args=env-platform\n"), "{output}");
        assert!(
            calls.is_empty(),
            "exported env must cost zero shdeps spawns, got:\n{calls}"
        );
    }

    /// Unset-or-empty exports fall back to the bridge, so probes without an
    /// export and hooks that unset the variables keep working. Flag queries
    /// have env-only semantics matching the bridge (a nested `__api` call
    /// carries no CLI overrides), so they never spawn.
    #[cfg(unix)]
    #[test]
    fn prelude_snapshot_queries_fall_back_to_bridge_when_unset() {
        let (output, calls) = run_prelude_driver(
            r#"
printf 'platform=%s\n' "$(shdeps_platform)"
shdeps_force; printf 'force=%s\n' "$?"
shdeps_reinstall; printf 'reinstall=%s\n' "$?"
printf 'pkg-mgr=%s\n' "$(shdeps_pkg_mgr)"
printf 'install-dir=%s\n' "$(shdeps_install_dir)"
printf 'git-dev-dir=%s\n' "$(shdeps_git_dev_dir)"
printf 'bin-dir=%s\n' "$(shdeps_bin_dir)"
printf 'args=%s\n' "$(shdeps_platform ignored-arg)"
"#,
            &[],
        );

        assert!(output.contains("platform=stub-platform\n"), "{output}");
        assert!(output.contains("force=1\n"), "{output}");
        assert!(output.contains("reinstall=1\n"), "{output}");
        assert!(output.contains("pkg-mgr=stub-pkg\n"), "{output}");
        assert!(output.contains("install-dir=/stub/install\n"), "{output}");
        assert!(output.contains("git-dev-dir=/stub/gitdev\n"), "{output}");
        assert!(output.contains("bin-dir=/stub/bin\n"), "{output}");
        assert!(output.contains("args=stub-platform\n"), "{output}");
        for expected in [
            "__api platform",
            "__api pkg-mgr",
            "__api install-dir",
            "__api git-dev-dir",
            "__api bin-dir",
            "__api platform ignored-arg",
        ] {
            assert!(
                calls.lines().any(|line| line == expected),
                "missing bridge call {expected:?} in:\n{calls}"
            );
        }
        assert!(
            !calls
                .lines()
                .any(|line| line == "__api force" || line == "__api reinstall"),
            "flag queries must never spawn, got:\n{calls}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prelude_empty_exports_fall_back_to_bridge() {
        // An empty platform export still bridges (the bridge would detect
        // the real platform), while an empty `SHDEPS_PKG_MGR` is already
        // the bridge's definitive "none detected" answer.
        let (output, calls) = run_prelude_driver(
            r#"
printf 'platform=%s\n' "$(shdeps_platform)"
printf 'pkg-mgr=%s\n' "$(shdeps_pkg_mgr)"
"#,
            &[("SHDEPS_HOOK_PLATFORM", ""), ("SHDEPS_PKG_MGR", "")],
        );

        assert!(output.contains("platform=stub-platform\n"), "{output}");
        assert!(output.contains("pkg-mgr=\n"), "{output}");
        assert!(
            calls.lines().any(|line| line == "__api platform"),
            "an empty platform export must fall back to the bridge, got:\n{calls}"
        );
        assert!(
            !calls.lines().any(|line| line == "__api pkg-mgr"),
            "an empty pkg-mgr export is definitive, got:\n{calls}"
        );
    }

    #[test]
    fn prelude_uses_bridge_helpers_for_mutating_api() {
        let source = source();

        assert!(
            source.contains(r#"shdeps_link_extras() { command shdeps __api link-extras "$@"; }"#)
        );
        assert!(source.contains(
            r#"shdeps_github_release_install() { command shdeps __api github-release-install "$@"; }"#
        ));
        assert!(source.contains(r#"shdeps_dep_links() { command shdeps __api dep-links "$@"; }"#));
        assert!(source.contains(r#"shdeps_skip() { command shdeps __api skip-mark "$@"; }"#));
        assert!(
            source.contains(r#"shdeps_find_runtime() { command shdeps __api find-runtime "$@"; }"#)
        );
        assert!(
            source
                .contains(r#"shdeps_write_wrapper() { command shdeps __api write-wrapper "$@"; }"#)
        );
        assert!(source.contains("shdeps_dep_source()"));
        assert!(source.contains("shdeps_mark_changed()"));
    }
}
