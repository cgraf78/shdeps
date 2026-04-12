# shellcheck shell=bash
# Example hook for the "nerd-fonts" custom dependency.
#
# Hook files are sourced by shdeps after install/update. Each file can
# define post() and/or status() functions.
#
# Place this file at: <hooks_dir>/nerd-fonts.sh
# (hooks_dir defaults to the same directory as deps.conf, under hooks.d/)

# post() — runs when the dependency is newly installed, updated, or forced.
# Return 0 to mark the hook as complete (stamps the TTL).
# Return non-zero to retry on the next run.
post() {
  local font_dir="$HOME/.local/share/fonts"
  local font_name="JetBrainsMono"
  local repo="ryanoasis/nerd-fonts"

  # Skip if already installed
  if ls "$font_dir"/"$font_name"*.ttf &>/dev/null; then
    _shdeps_log "  nerd-fonts: $font_name already installed"
    return 0
  fi

  _shdeps_log "  nerd-fonts: installing $font_name..."
  mkdir -p "$font_dir"

  local url="https://github.com/$repo/releases/latest/download/$font_name.tar.xz"
  if curl -fsSL "$url" | tar xJ -C "$font_dir" 2>/dev/null; then
    # Rebuild font cache on Linux
    if command -v fc-cache &>/dev/null; then
      fc-cache -f "$font_dir" 2>/dev/null || true
    fi
    _shdeps_log_ok "  nerd-fonts: $font_name installed"
    return 0
  else
    _shdeps_warn "  nerd-fonts: failed to install $font_name"
    return 1
  fi
}

# status() — runs every time (read-only reporting).
# Use this to print current state without making changes.
status() {
  local font_dir="$HOME/.local/share/fonts"
  if ls "$font_dir"/JetBrainsMono*.ttf &>/dev/null; then
    local count
    count=$(find "$font_dir" -name 'JetBrainsMono*.ttf' -maxdepth 1 2>/dev/null | wc -l)
    _shdeps_log_dim "  nerd-fonts: $count font files installed"
  fi
}
