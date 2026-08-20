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

shdeps_platform() { command shdeps __api platform "$@"; }
shdeps_force() { command shdeps __api force "$@"; }
shdeps_reinstall() { command shdeps __api reinstall "$@"; }
shdeps_pkg_mgr() { command shdeps __api pkg-mgr "$@"; }
shdeps_pkg_install() { command shdeps __api pkg-install "$@"; }
shdeps_pkg_install_for_mgr() { command shdeps __api pkg-install-for-mgr "$@"; }
shdeps_require_sudo() { command shdeps __api require-sudo "$@"; }
shdeps_install_dir() { command shdeps __api install-dir "$@"; }
shdeps_git_dev_dir() { command shdeps __api git-dev-dir "$@"; }
shdeps_bin_dir() { command shdeps __api bin-dir "$@"; }

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
