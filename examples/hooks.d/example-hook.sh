# shellcheck shell=bash
# Example hook for the "nerd-fonts" custom dependency.
#
# Hook files are sourced by shdeps for custom deps. Each file can define:
#
#   exists()   — return 0 if installed, 1 if missing. Required.
#   version()  — print version string to stdout. Optional.
#   install()  — perform the install unconditionally. Return 0 on success.
#   post()     — optional post-install setup (runs if dep changed).
#
# shdeps calls exists() to decide whether to run install(), and uses
# version() for the status line. Hook authors don't need to check
# shdeps_force or shdeps_reinstall — shdeps handles gating.
#
# Place this file at: <hooks_dir>/nerd-fonts.sh
# (hooks_dir defaults to <conf_dir>/hooks.d/)
#
# Public API available to hooks:
#   $1                    Dependency name (passed to all hook functions)
#   shdeps_log            Normal log line
#   shdeps_warn           Warning (always shown unless quiet)
#   shdeps_log_ok         Success highlight
#   shdeps_log_dim        Dimmed / low-importance line
#   shdeps_log_header     Section header
#   shdeps_pkg_mgr        Detected package manager (brew/apt/dnf/pacman/"")
#   shdeps_force          Returns 0 if force mode is active (TTL bypass)
#   shdeps_reinstall      Returns 0 if reinstall mode is active
#   shdeps_platform       Normalized platform name (linux, macos, wsl)
#   shdeps_require_sudo   Acquire sudo (returns 0 if root or sudo obtained)
#   shdeps_platform_match Check if current platform matches a spec
#   shdeps_host_match     Check if current hostname matches a spec

# exists() — return 0 if the dep is installed, 1 if missing.
exists() {
  local font_dir="$HOME/.local/share/fonts"
  ls "$font_dir"/JetBrainsMono*.ttf &>/dev/null
}

# version() — print the version string.
# Omit for deps without a single meaningful version.

# install() — perform the install unconditionally.
# shdeps only calls this when exists() returns 1 (or --reinstall).
install() {
  local name="$1"
  local font_dir="$HOME/.local/share/fonts"
  local font_name="JetBrainsMono"
  local repo="ryanoasis/nerd-fonts"

  shdeps_log "  $name: installing $font_name..."
  mkdir -p "$font_dir"

  local url="https://github.com/$repo/releases/latest/download/$font_name.tar.xz"
  if curl -fsSL "$url" | tar xJ -C "$font_dir" 2>/dev/null; then
    if command -v fc-cache &>/dev/null; then
      fc-cache -f "$font_dir" 2>/dev/null || true
    fi
    return 0
  else
    return 1
  fi
}

# -----------------------------------------------------------------------------
# Example: regenerate completions for a cargo/go dep after install
# -----------------------------------------------------------------------------
# For cargo/go deps, shdeps does not auto-discover man pages or shell
# completions (the install produces a single binary only). Generate them
# from the tool itself in post(). Save as hooks.d/ripgrep.sh for a
# `ripgrep cargo rg` entry:
#
# post() {
#   local comp="$HOME/.local/share/bash-completion/completions/rg"
#   mkdir -p "$(dirname "$comp")"
#   rg --generate=complete-bash > "$comp"
# }
