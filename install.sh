#!/usr/bin/env bash
# Install or update shdeps.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/cgraf78/shdeps/main/install.sh | bash
#   SHDEPS_DIR=~/.local/share/shdeps ./install.sh
#   ./install.sh --uninstall
#
# Environment:
#   SHDEPS_DIR    Install directory (default: ~/.local/share/shdeps)
#   SHDEPS_REPO   Git repo URL (default: https://github.com/cgraf78/shdeps.git)

set -euo pipefail

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

  # Symlink CLI into PATH
  if [[ -x "$SHDEPS_DIR/bin/shdeps" ]]; then
    mkdir -p "$(dirname "$SHDEPS_BIN")"
    ln -sf "$SHDEPS_DIR/bin/shdeps" "$SHDEPS_BIN"
  fi

  # Hint if ~/.local/bin isn't on PATH
  case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *)
    _info ""
    _info "Add ~/.local/bin to your PATH if it isn't already:"
    _info "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
  esac
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

_uninstall() {
  local removed=0
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
--uninstall) _uninstall ;;
"")          _install ;;
*)
  _error "unknown argument: $1"
  _info "Usage: install.sh [--uninstall]"
  exit 2
  ;;
esac
