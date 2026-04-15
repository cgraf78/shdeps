#!/usr/bin/env bash
# Install, update, or bootstrap shdeps.
#
# Usage:
#   curl -fsSL .../install.sh | bash          # install/update
#   . /path/to/install.sh --bootstrap         # source into caller
#   ./install.sh --uninstall                  # remove
#
# Environment:
#   SHDEPS_DIR          Install directory      (default: ~/.local/share/shdeps)
#   SHDEPS_REPO         Git repo URL           (default: https://github.com/cgraf78/shdeps.git)
#   SHDEPS_BIN          CLI symlink path       (default: ~/.local/bin/shdeps)
#   SHDEPS_LIB          Direct path to shdeps.sh (skips discovery in --bootstrap)
#   SHDEPS_GIT_DEV_DIR  Dev clone directory    (default: ~/git)

# Strict mode when executed directly; skip when sourced (--bootstrap)
# to avoid infecting the caller's shell options.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
fi

SHDEPS_DIR="${SHDEPS_DIR:-$HOME/.local/share/shdeps}"
SHDEPS_REPO="${SHDEPS_REPO:-https://github.com/cgraf78/shdeps.git}"
SHDEPS_BIN="${SHDEPS_BIN:-$HOME/.local/bin/shdeps}"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_info()  { printf '%s\n' "$*" >&2; }
_error() { printf 'error: %s\n' "$*" >&2; }

_check_prereqs() {
  if ! command -v git &>/dev/null; then
    _error "git is required"
    exit 1
  fi
  if [[ "${BASH_VERSINFO[0]}" -lt 4 ]]; then
    _error "bash 4+ is required (found ${BASH_VERSION})"
    exit 1
  fi
}

# Symlink CLI into PATH and link man page + shell completions.
# Requires shdeps.sh to be sourced first (for _shdeps_link_extras).
_setup_links() {
  local shdeps_dir="$1"

  if [[ -x "$shdeps_dir/bin/shdeps" ]]; then
    mkdir -p "$(dirname "$SHDEPS_BIN")"
    ln -sf "$shdeps_dir/bin/shdeps" "$SHDEPS_BIN"
  fi

  if declare -f _shdeps_link_extras &>/dev/null; then
    _shdeps_link_extras "shdeps" "$shdeps_dir"
  fi
}

# ---------------------------------------------------------------------------
# Install / update
# ---------------------------------------------------------------------------

_install() {
  _check_prereqs

  if [[ -d "$SHDEPS_DIR/.git" ]]; then
    # Already installed — pull latest if clean
    if [[ -n "$(git -C "$SHDEPS_DIR" status --porcelain --untracked-files=normal 2>/dev/null)" ]]; then
      _info "shdeps: dirty working tree, skipping update"
    elif git -C "$SHDEPS_DIR" pull --ff-only --quiet 2>&1; then
      _info "shdeps: updated"
    else
      _error "shdeps: update failed (git pull --ff-only failed)"
      exit 1
    fi
  elif [[ -d "$SHDEPS_DIR" ]]; then
    _error "$SHDEPS_DIR exists but is not a git repo"
    exit 1
  else
    _info "shdeps: cloning to $SHDEPS_DIR..."
    git clone --depth 1 "$SHDEPS_REPO" "$SHDEPS_DIR"
    _info "shdeps: installed"
  fi

  # Source the library and set up all symlinks (CLI, man, completions)
  if [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    # shellcheck source=/dev/null
    . "$SHDEPS_DIR/shdeps.sh"
    _setup_links "$SHDEPS_DIR"
  fi

  # Hint if the bin directory isn't on PATH
  local bin_dir
  bin_dir=$(dirname "$SHDEPS_BIN")
  case ":$PATH:" in
  *":${bin_dir}:"*) ;;
  *)
    _info ""
    _info "Add $bin_dir to your PATH if it isn't already:"
    _info "  export PATH=\"${bin_dir}:\$PATH\""
    ;;
  esac
}

# ---------------------------------------------------------------------------
# Bootstrap — source shdeps into the caller
# ---------------------------------------------------------------------------
# Designed to be sourced: `. /path/to/install.sh --bootstrap`
#
# Finds shdeps.sh, sources it, symlinks the CLI, and runs self-update.
# Clients set env vars (SHDEPS_CONF_DIR, SHDEPS_HOOKS_DIR, etc.) before
# sourcing. Pre-defined _shdeps_log* functions are respected by shdeps.sh.
#
# Returns 0 if shdeps is ready, 1 if bootstrap failed.

_bootstrap() {
  # Idempotent — skip if already bootstrapped
  declare -f shdeps_update &>/dev/null && return 0

  local _bs_lib="" _bs_dir=""
  local _dev_dir="${SHDEPS_GIT_DEV_DIR:-$HOME/git}"

  # Find shdeps.sh: env override → dev clone → installed clone → fresh install
  if [[ -n "${SHDEPS_LIB:-}" && -f "$SHDEPS_LIB" ]]; then
    _bs_lib="$SHDEPS_LIB"
    _bs_dir="${SHDEPS_LIB%/*}"
  elif [[ -f "$_dev_dir/shdeps/shdeps.sh" ]]; then
    _bs_lib="$_dev_dir/shdeps/shdeps.sh"
    _bs_dir="$_dev_dir/shdeps"
  elif [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    _bs_lib="$SHDEPS_DIR/shdeps.sh"
    _bs_dir="$SHDEPS_DIR"
  else
    # Not installed — run _install in a subshell so exit doesn't kill caller
    # shellcheck disable=SC2310  # intentional: subshell contains exit
    if ( _install ) >/dev/null 2>&1; then
      _bs_lib="$SHDEPS_DIR/shdeps.sh"
      _bs_dir="$SHDEPS_DIR"
    else
      return 1
    fi
  fi

  # Source the library
  # shellcheck source=/dev/null
  . "$_bs_lib" || return 1

  # Set up all symlinks (CLI, man, completions)
  [[ -n "$_bs_dir" ]] && _setup_links "$_bs_dir"

  # Pull latest shdeps (skips dirty clones / active development).
  # Self-update re-links extras after pulling, so changes are picked up.
  if [[ -n "$_bs_dir" ]] && declare -f _shdeps_self_update &>/dev/null; then
    _shdeps_self_update "$_bs_dir" 2>/dev/null || true
  fi
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

_uninstall() {
  local removed=0

  # Clean up extras symlinks (man page, completions) before removing the repo
  if [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    # shellcheck source=/dev/null
    . "$SHDEPS_DIR/shdeps.sh"
    _shdeps_unlink_extras "shdeps"
  fi

  if [[ -L "$SHDEPS_BIN" ]]; then
    rm "$SHDEPS_BIN"
    ((removed++)) || true
  fi
  if [[ -d "$SHDEPS_DIR" ]]; then
    rm -rf "$SHDEPS_DIR"
    ((removed++)) || true
  fi
  if [[ $removed -gt 0 ]]; then
    _info "shdeps: uninstalled"
  else
    _info "shdeps: nothing to uninstall"
  fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

case "${1:-}" in
--uninstall)  _uninstall ;;
--bootstrap)  _bootstrap ;;
"")           _install ;;
*)
  _error "unknown argument: $1"
  _info "Usage: install.sh [--uninstall|--bootstrap]"
  exit 2
  ;;
esac
