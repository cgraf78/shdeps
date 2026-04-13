# shellcheck shell=bash
# Example hook for the "nerd-fonts" custom dependency.
#
# Hook files are sourced by shdeps after install/update. Each file can
# define post() and/or status() functions.
#
# Place this file at: <hooks_dir>/nerd-fonts.sh
# (hooks_dir defaults to <conf_dir>/hooks.d/)
#
# Public API available to hooks:
#   $1                    Dependency name (passed to both post and status)
#   shdeps_log            Normal log line
#   shdeps_warn           Warning (always shown unless quiet)
#   shdeps_log_ok         Success highlight
#   shdeps_log_dim        Dimmed / low-importance line
#   shdeps_log_header     Section header
#   shdeps_pkg_mgr        Detected package manager (brew/apt/dnf/pacman/"")
#   shdeps_force          Returns 0 if force mode is active
#   shdeps_platform       Normalized platform name (linux, macos, wsl)
#   shdeps_require_sudo   Acquire sudo (returns 0 if root or sudo obtained)
#   shdeps_platform_match Check if current platform matches a spec
#   shdeps_host_match     Check if current hostname matches a spec

# install() — runs for custom deps when the hook is due.
# $1 is the dependency name. This IS the installer for custom deps.
# Return 0 to mark the hook as complete (stamps the TTL).
# Return non-zero to retry on the next run.
install() {
  local name="$1"
  local font_dir="$HOME/.local/share/fonts"
  local font_name="JetBrainsMono"
  local repo="ryanoasis/nerd-fonts"

  # Skip if already installed
  if ls "$font_dir"/"$font_name"*.ttf &>/dev/null; then
    shdeps_log "  $name: $font_name already installed"
    return 0
  fi

  shdeps_log "  $name: installing $font_name..."
  mkdir -p "$font_dir"

  local url="https://github.com/$repo/releases/latest/download/$font_name.tar.xz"
  if curl -fsSL "$url" | tar xJ -C "$font_dir" 2>/dev/null; then
    # Rebuild font cache on Linux
    if command -v fc-cache &>/dev/null; then
      fc-cache -f "$font_dir" 2>/dev/null || true
    fi
    shdeps_log_ok "  $name: $font_name installed"
    return 0
  else
    shdeps_warn "  $name: failed to install $font_name"
    return 1
  fi
}

# status() — runs every time (read-only reporting).
# $1 is the dependency name.
# Use this to print current state without making changes.
status() {
  local name="$1"
  local font_dir="$HOME/.local/share/fonts"
  if ls "$font_dir"/JetBrainsMono*.ttf &>/dev/null; then
    local count
    count=$(find "$font_dir" -name 'JetBrainsMono*.ttf' -maxdepth 1 2>/dev/null | wc -l)
    shdeps_log_dim "  $name: $count font files installed"
  fi
}
