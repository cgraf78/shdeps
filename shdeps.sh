# shellcheck shell=bash
# shdeps — Rust compatibility wrapper.
#
# This file is the sourceable Bash API for the Rust implementation. Keep it
# intentionally small: every substantial operation should live in Rust and be
# reached through the public CLI or hidden `shdeps __api` bridge. That keeps the
# sourceable contract available for dotfiles/hooks without letting a second
# Bash implementation drift from Rust.

_shdepsw_source_fail() {
  printf 'shdeps: %s\n' "$*" >&2
  return 1
}

_SHDEPSW_SOURCED=0
(return 0 2>/dev/null) && _SHDEPSW_SOURCED=1

if [[ -z "${BASH_VERSION:-}" ]]; then
  _shdepsw_source_fail "shdeps.sh must be sourced by Bash 4.3 or newer"
  if [[ "$_SHDEPSW_SOURCED" == "1" ]]; then return 1; fi
  exit 1
fi

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 3))); then
  _shdepsw_source_fail "shdeps.sh requires Bash 4.3 or newer; run the shdeps CLI directly under older shells"
  if [[ "$_SHDEPSW_SOURCED" == "1" ]]; then return 1; fi
  exit 1
fi

_shdepsw_dir() {
  local src="${BASH_SOURCE[0]}" dir
  case "$src" in
    */*) dir="${src%/*}" ;;
    *) dir="." ;;
  esac
  cd -P -- "$dir" 2>/dev/null && pwd
}

_shdepsw_resolve_binary() {
  local dir candidate

  # Tests and bridge experiments need an explicit override before release
  # archives are the only source of this wrapper. Keep the variable test-facing:
  # production installs should normally use the sibling binary below.
  if [[ -n "${SHDEPS_RUST_CLI:-}" && -x "${SHDEPS_RUST_CLI:-}" ]]; then
    printf '%s\n' "$SHDEPS_RUST_CLI"
    return 0
  fi

  dir=$(_shdepsw_dir) || return 1
  candidate="$dir/shdeps"
  if [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  # Developer checkouts do not have a sibling release binary until packaged,
  # but they commonly have a Cargo-built binary. Prefer debug first because it
  # is what `cargo build` and the local test harness produce; release remains a
  # useful fallback for manual smoke testing.
  candidate="$dir/target/debug/shdeps"
  if [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  candidate="$dir/target/release/shdeps"
  if [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  # Last-resort fallback: any `shdeps` found on `$PATH`. This is opt-in
  # because in shared environments (CI containers, multi-tenant
  # devservers) the `shdeps` on PATH may belong to a different user or
  # repo than the `shdeps.sh` being sourced. Silently picking it up has
  # caused mysterious config mismatches and the wrong-binary's ABI to
  # leak into the caller. Operators who genuinely want that fallback
  # opt in with `SHDEPS_ALLOW_PATH_FALLBACK=1`.
  if [[ "${SHDEPS_ALLOW_PATH_FALLBACK:-0}" == "1" ]]; then
    command -v shdeps 2>/dev/null
  fi
}

_SHDEPSW_BIN="$(_shdepsw_resolve_binary)" || {
  _shdepsw_source_fail "could not find the Rust shdeps binary for this wrapper"
  if [[ "$_SHDEPSW_SOURCED" == "1" ]]; then return 1; fi
  exit 1
}

_shdepsw_call() {
  # Lazy-init the ABI handshake and env-snapshot cache on first use
  # instead of at source time. Sourcing this file used to spawn two
  # `shdeps __api ...` subprocesses on every cold shell startup even
  # when the user never invoked any `shdeps_*` function in that shell.
  # The `_SHDEPS_ABI_CHECKED` / `_SHDEPSW_ENV_CACHED` guards inside
  # `_shdepsw_ensure_abi` / `_shdepsw_cache_env` make subsequent calls
  # near-free, so every binary-backed public function still gets the
  # safety check, just at the point where the work is actually needed.
  _shdepsw_lazy_init || return $?
  command "$_SHDEPSW_BIN" "$@"
}

_shdepsw_lazy_init() {
  _shdepsw_ensure_abi || return $?
  _shdepsw_cache_env || return $?
}

_shdepsw_prepend_bin_dir() {
  local bin_dir
  bin_dir="${SHDEPS_BIN_DIR:-${_SHDEPSW_BIN_DIR:-$(_shdepsw_call __api bin-dir)}}"
  case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *)
      # The Bash implementation updated the caller's PATH before `update` so
      # freshly installed release/toolchain binaries were visible to later
      # hooks in the same `dot update` or bootstrap process. Rust can only
      # mutate its child environment, so the sourceable wrapper preserves that
      # parent-shell contract here and leaves all install behavior in Rust.
      export PATH="$bin_dir:${PATH:-}"
      ;;
  esac
}

_shdepsw_ensure_abi() {
  local version
  [[ "${_SHDEPS_ABI_CHECKED:-}" == "1" ]] && return 0

  # Invoke the binary directly here instead of through `_shdepsw_call`.
  # `_shdepsw_call` now triggers lazy init, which calls this function —
  # routing through it would recurse indefinitely. The whole point of
  # the lazy-init layer is to defer this single subprocess until first
  # use, so calling the binary by path is exactly correct.
  version=$(command "$_SHDEPSW_BIN" __api version 2>/dev/null) || {
    _shdepsw_source_fail "Rust shdeps binary does not expose the wrapper ABI"
    return 1
  }
  case "$version" in
    abi:1)
      _SHDEPS_ABI_CHECKED=1
      ;;
    *)
      _shdepsw_source_fail "Rust shdeps binary reports unsupported wrapper ABI: $version"
      ;;
  esac
}

_shdepsw_cache_env() {
  local snapshot line key value
  [[ "${_SHDEPSW_ENV_CACHED:-}" == "1" ]] && return 0

  # Same recursion concern as `_shdepsw_ensure_abi`: call the binary
  # directly rather than through `_shdepsw_call`.
  snapshot=$(command "$_SHDEPSW_BIN" __api env-snapshot) || return $?
  while IFS= read -r line; do
    key=${line%%=*}
    value=${line#*=}
    case "$key" in
      install_dir) _SHDEPSW_INSTALL_DIR=$value ;;
      bin_dir) _SHDEPSW_BIN_DIR=$value ;;
      git_dev_dir) _SHDEPSW_GIT_DEV_DIR=$value ;;
      platform) _SHDEPSW_PLATFORM=$value ;;
      pkg_mgr) _SHDEPSW_PKG_MGR=$value ;;
      force) _SHDEPSW_FORCE=$value ;;
      reinstall) _SHDEPSW_REINSTALL=$value ;;
    esac
  done <<<"$snapshot"

  # Export the package-manager cache when one exists because hidden `__api`
  # subprocesses cannot see private Bash variables. An empty value is not
  # exported: empty means "not detected yet", and exporting it would erase a
  # caller's later explicit SHDEPS_PKG_MGR override.
  if [[ -n "${_SHDEPSW_PKG_MGR:-}" ]]; then
    export SHDEPS_PKG_MGR="$_SHDEPSW_PKG_MGR"
  fi
  _SHDEPSW_ENV_CACHED=1
}

_shdepsw_should_log() {
  [[ "${SHDEPS_LOG_LEVEL:-1}" -ge 1 && "${SHDEPS_QUIET:-0}" -ne 1 ]]
}

_shdepsw_valid_marker_component() {
  local value="$1"
  [[ -n "$value" && "$value" != "." && "$value" != ".." && "$value" != *[[:space:]]* && "$value" != */* && "$value" != *\\* ]]
}

_shdepsw_valid_dep_name() {
  local name="$1"
  [[ -n "$name" && "$name" != /* && "$name" != *\\* && "$name" != "." && "$name" != ".." && "$name" != ../* && "$name" != */../* && "$name" != */.. && "$name" != *\|* ]]
}

_shdepsw_state_dir() {
  printf '%s\n' "${SHDEPS_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/shdeps}"
}

# ---------------------------------------------------------------------------
# Public API — stable sourceable interface for callers and hook authors.
# ---------------------------------------------------------------------------
# Keep these shims deliberately boring. Any behavior that can run in a child
# process goes through the Rust binary, and the few current-shell exceptions
# are documented inline because they cannot be represented by a subprocess
# alone.

shdeps_version() { _shdepsw_call version "$@"; }
shdeps_update() { _shdepsw_prepend_bin_dir && _shdepsw_call update "$@"; }
shdeps_self_update() { _shdepsw_call self-update "$@"; }
shdeps_load() { _shdepsw_call __api load-count "$@"; }
shdeps_prune() { _shdepsw_call prune "$@"; }

shdeps_platform_match() { _shdepsw_call __api platform-match "$@"; }
shdeps_host_match() { _shdepsw_call __api host-match "$@"; }
shdeps_filter_match() { _shdepsw_call __api filter-match "$@"; }

shdeps_platform() { printf '%s\n' "${_SHDEPSW_PLATFORM:-$(_shdepsw_call __api platform)}"; }
# `shdeps_force` / `shdeps_reinstall` / `shdeps_pkg_mgr` read cached env
# vars populated by `_shdepsw_cache_env`. With lazy init they need to
# trigger the cache themselves; otherwise the first call after sourcing
# would compare against an empty cache and silently report the wrong
# default. `_shdepsw_lazy_init` is gated by a one-shot flag so the
# repeated calls cost nothing after the first.
shdeps_force() {
  _shdepsw_lazy_init || return $?
  [[ "${SHDEPS_FORCE:-${_SHDEPSW_FORCE:-0}}" == "1" ]]
}
shdeps_reinstall() {
  _shdepsw_lazy_init || return $?
  [[ "${SHDEPS_REINSTALL:-${_SHDEPSW_REINSTALL:-0}}" == "1" ]]
}
shdeps_pkg_mgr() {
  _shdepsw_lazy_init || return $?
  printf '%s\n' "${_SHDEPS_PKG_MGR:-${SHDEPS_PKG_MGR:-${_SHDEPSW_PKG_MGR:-}}}"
}
shdeps_pkg_install() { SHDEPS_PKG_MGR="$(shdeps_pkg_mgr)" _shdepsw_call __api pkg-install "$@"; }
shdeps_pkg_install_for_mgr() { SHDEPS_PKG_MGR="$(shdeps_pkg_mgr)" _shdepsw_call __api pkg-install-for-mgr "$@"; }
shdeps_require_sudo() { _shdepsw_call __api require-sudo "$@"; }
shdeps_install_dir() { printf '%s\n' "${SHDEPS_INSTALL_DIR:-${_SHDEPSW_INSTALL_DIR:-$(_shdepsw_call __api install-dir)}}"; }
shdeps_git_dev_dir() { printf '%s\n' "${SHDEPS_GIT_DEV_DIR:-${_SHDEPSW_GIT_DEV_DIR:-$(_shdepsw_call __api git-dev-dir)}}"; }
shdeps_bin_dir() { printf '%s\n' "${SHDEPS_BIN_DIR:-${_SHDEPSW_BIN_DIR:-$(_shdepsw_call __api bin-dir)}}"; }

shdeps_dep_root() { _shdepsw_call __api dep-root "$@"; }
shdeps_dep_path() { _shdepsw_call __api dep-path "$@"; }
shdeps_dep_file() { _shdepsw_call __api dep-file "$@"; }
shdeps_dep_source() {
  local source_path
  source_path=$(_shdepsw_call __api dep-file "$@") || return $?
  # This helper must source into the caller's shell so completions/env changes
  # take effect immediately. Everything else should stay in Rust subprocesses.
  # shellcheck source=/dev/null
  . "$source_path"
}

shdeps_link_extras() { _shdepsw_call __api link-extras "$@"; }
shdeps_unlink_extras() { _shdepsw_call __api unlink-extras "$@"; }
shdeps_github_release_install() { _shdepsw_call __api github-release-install "$@"; }

shdeps_log() { if _shdepsw_should_log; then printf '%s\n' "$*"; fi; }
shdeps_warn() { if _shdepsw_should_log; then printf '%s\n' "$*" >&2; fi; }
shdeps_log_warn() { shdeps_warn "$@"; }
shdeps_log_ok() { shdeps_log "$@"; }
shdeps_log_dim() { shdeps_log "$@"; }
shdeps_log_header() { shdeps_log "$@"; }

shdeps_mark_changed() {
  local txn="${SHDEPS_UPDATE_TXN_ID:-}" name="${1:-}" state_dir marker
  _shdepsw_valid_marker_component "$txn" || return 0
  _shdepsw_valid_dep_name "$name" || return 0

  # The parent Rust update process collects these sentinel files after hook
  # subprocesses exit. Writing the marker here preserves the current-shell Bash
  # API without requiring hooks to know about Rust's coordination directory.
  state_dir=$(_shdepsw_state_dir)
  marker="$state_dir/.changed-markers/$txn/$name"
  mkdir -p "${marker%/*}" || return 1
  : >"$marker"
}
