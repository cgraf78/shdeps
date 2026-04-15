# shellcheck shell=bash
# shdeps — standalone shell dependency manager.
#
# Reads declarative config files (*.conf) from a config directory and
# installs/updates tools via system package managers (brew/apt/dnf/pacman),
# GitHub git repos, or GitHub release binaries. Post-install hooks run
# arbitrary setup after changes.
#
# Usage:
#   source shdeps.sh
#   shdeps_update
#
# Configuration (env vars, all optional):
#   SHDEPS_CONF_DIR     Config directory            (default: ./shdeps)
#   SHDEPS_HOOKS_DIR    Post-install hooks dir      (default: <conf_dir>/hooks.d)
#   SHDEPS_STATE_DIR    Cache/state directory       (default: $XDG_STATE_HOME/shdeps)
#   SHDEPS_FORCE        Bypass TTL cache            (default: 0)
#   SHDEPS_REINSTALL    Force reinstall all deps    (default: 0)
#   SHDEPS_QUIET        Suppress interactive prompts(default: 0)
#   SHDEPS_REMOTE_TTL   Cache TTL in seconds        (default: 3600)
#   SHDEPS_GIT_DEV_DIR  Dev clone directory          (default: ~/git)
#   SHDEPS_INSTALL_DIR  Base dir for git/binary installs (default: ~/.local/share)
#   SHDEPS_BIN_DIR      Directory for binary symlinks (default: ~/.local/bin)
#   SHDEPS_LOG_LEVEL    0=quiet, 1=normal, 2=verbose(default: 1)

SHDEPS_VERSION="$(cat "${BASH_SOURCE[0]%/*}/VERSION" 2>/dev/null || echo unknown)"

# ---------------------------------------------------------------------------
# Public API — stable interface for callers and hook authors
# ---------------------------------------------------------------------------
# Every shdeps_ function (no leading underscore) is defined here.  This is
# the complete public contract.  Internal implementations live in later
# sections with _shdeps_ prefixes.

# Core
shdeps_version()        { echo "shdeps $SHDEPS_VERSION"; }
shdeps_update()         { _shdeps_update "$@"; }
shdeps_self_update()    { _shdeps_self_update "$@"; }
shdeps_load()           { _shdeps_load; echo "${#_SHDEPS_DEPS[@]}"; }
shdeps_prune()          { _shdeps_prune "$@"; }

# Matching
shdeps_platform_match() { _shdeps_platform_match "$@"; }
shdeps_host_match()     { _shdeps_host_match "$@"; }

# Platform and environment
shdeps_platform() {
  local current
  current=$(uname -s | tr '[:upper:]' '[:lower:]')
  if _shdeps_is_wsl; then current="wsl"; fi
  if [[ "$current" == "darwin" ]]; then current="macos"; fi
  echo "$current"
}
shdeps_force()          { [[ "$(_shdeps_force)" -eq 1 ]]; }
shdeps_reinstall()      { [[ "$(_shdeps_reinstall)" -eq 1 ]]; }
shdeps_pkg_mgr()        { echo "${_SHDEPS_PKG_MGR:-}"; }
shdeps_require_sudo()   { _shdeps_require_sudo; }
shdeps_install_dir()    { _shdeps_install_dir; }
shdeps_git_dev_dir()    { _shdeps_git_dev_dir; }
shdeps_bin_dir()        { _shdeps_bin_dir; }

# Logging
shdeps_log()            { _shdeps_log "$@"; }
shdeps_warn()           { _shdeps_warn "$@"; }
shdeps_log_ok()         { _shdeps_log_ok "$@"; }
shdeps_log_dim()        { _shdeps_log_dim "$@"; }
shdeps_log_header()     { _shdeps_log_header "$@"; }

# ===========================================================================
# Internal implementation — everything below is private (_shdeps_ prefix)
# ===========================================================================

# ---------------------------------------------------------------------------
# Configuration defaults
# ---------------------------------------------------------------------------

# Return the config directory (normalized to absolute path).
_shdeps_conf_dir() {
  local dir="${SHDEPS_CONF_DIR:-./shdeps}"
  if [[ "$dir" != /* ]]; then
    dir="$(cd "$dir" 2>/dev/null && pwd)" || dir="$(pwd)/$dir"
  fi
  echo "$dir"
}

_shdeps_hooks_dir()  { echo "${SHDEPS_HOOKS_DIR:-$(_shdeps_conf_dir)/hooks.d}"; }
_shdeps_state_dir()  { echo "${SHDEPS_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/shdeps}"; }
_shdeps_force()      { echo "${SHDEPS_FORCE:-0}"; }
_shdeps_reinstall()  { echo "${SHDEPS_REINSTALL:-0}"; }
_shdeps_quiet()      { echo "${SHDEPS_QUIET:-0}"; }
_shdeps_remote_ttl() { echo "${SHDEPS_REMOTE_TTL:-3600}"; }
_shdeps_git_dev_dir() { echo "${SHDEPS_GIT_DEV_DIR:-$HOME/git}"; }
_shdeps_install_dir() { echo "${SHDEPS_INSTALL_DIR:-$HOME/.local/share}"; }
_shdeps_bin_dir()     { echo "${SHDEPS_BIN_DIR:-$HOME/.local/bin}"; }
_shdeps_log_level()  { echo "${SHDEPS_LOG_LEVEL:-1}"; }

# ---------------------------------------------------------------------------
# Logging — callers may override by defining these before sourcing
# ---------------------------------------------------------------------------

# ANSI color codes, enabled only when stdout is a terminal.
# Skip if already set — re-sourcing (e.g. self-update) may happen under
# stdout redirection, which would incorrectly clear the codes.
if [[ -z "${_SHDEPS_C_RESET+x}" ]]; then
  if [[ -t 1 ]]; then
    _SHDEPS_C_RESET=$'\033[0m'
    _SHDEPS_C_RED=$'\033[0;31m'
    _SHDEPS_C_GREEN=$'\033[0;32m'
    _SHDEPS_C_DIM=$'\033[0;90m'
    _SHDEPS_C_BOLD=$'\033[1m'
    _SHDEPS_C_CLEARLN=$'\r\033[2K'
  else
    _SHDEPS_C_RESET="" _SHDEPS_C_RED="" _SHDEPS_C_GREEN=""
    _SHDEPS_C_DIM="" _SHDEPS_C_BOLD="" _SHDEPS_C_CLEARLN=""
  fi
fi

_SHDEPS_STATUS_ACTIVE=0

# Clear any active status line before printing a permanent log line.
_shdeps_log_clear() {
  if [[ $_SHDEPS_STATUS_ACTIVE -eq 1 ]]; then
    printf '%s' "${_SHDEPS_C_CLEARLN}"
    _SHDEPS_STATUS_ACTIVE=0
  fi
}

# Temporary in-place status (overwritten by the next log line).
# No-op when stdout is not a terminal or log level is 0.
_shdeps_log_status() {
  if [[ -z "$_SHDEPS_C_CLEARLN" ]] || ! _shdeps_should_log; then return 0; fi
  printf '%s%s%s%s' "${_SHDEPS_C_CLEARLN}" "${_SHDEPS_C_DIM}" "$*" "${_SHDEPS_C_RESET}"
  _SHDEPS_STATUS_ACTIVE=1
}

# Log functions — callers may override by redefining these after sourcing.

# Should this log line be shown? Checks log level and quiet mode.
_shdeps_should_log() {
  [[ "$(_shdeps_log_level)" -ge 1 && "$(_shdeps_quiet)" -ne 1 ]]
}

# Normal log line (level >= 1, not quiet).
_shdeps_log() {
  _shdeps_log_clear
  if _shdeps_should_log; then printf '%s\n' "$*"; fi
  return 0
}

# Warning (red, always to stderr).
_shdeps_warn() {
  _shdeps_log_clear
  if _shdeps_should_log; then
    printf '%s%s%s\n' "${_SHDEPS_C_RED}" "$*" "${_SHDEPS_C_RESET}" >&2
  fi
  return 0
}

# Success highlight.
_shdeps_log_ok() {
  _shdeps_log_clear
  if _shdeps_should_log; then
    printf '%s%s%s\n' "${_SHDEPS_C_GREEN}" "$*" "${_SHDEPS_C_RESET}"
  fi
  return 0
}

# Dimmed / low-importance line.
_shdeps_log_dim() {
  _shdeps_log_clear
  if _shdeps_should_log; then
    printf '%s%s%s\n' "${_SHDEPS_C_DIM}" "$*" "${_SHDEPS_C_RESET}"
  fi
  return 0
}

# Section header.
_shdeps_log_header() {
  _shdeps_log_clear
  if _shdeps_should_log; then
    printf '%s%s%s\n' "${_SHDEPS_C_BOLD}" "$*" "${_SHDEPS_C_RESET}"
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Utility helpers
# ---------------------------------------------------------------------------

# Detect Windows Subsystem for Linux.
_shdeps_is_wsl() {
  [[ -f /proc/version ]] || return 1
  grep -qi microsoft /proc/version 2>/dev/null
}

# Create a temp file for capturing command output. Sets REPLY to the path.
_shdeps_logfile_create() {
  REPLY=$(mktemp 2>/dev/null) || return 1
}

# Print the contents of a log file as a warning, then clean up.
# $1=label $2=logfile path
_shdeps_logfile_print() {
  local label="$1" log="$2"
  if [[ -s "$log" ]]; then
    _shdeps_warn "  --- $label output ---"
    while IFS= read -r line; do
      _shdeps_warn "  $line"
    done <"$log"
    _shdeps_warn "  --- end $label output ---"
  fi
}

# Run a command, capturing stdout+stderr to the current log file.
# The log file path is taken from the $log variable in the caller's scope.
_shdeps_run_logged() {
  if [[ -n "${log:-}" ]]; then
    "$@" >>"$log" 2>&1
  else
    "$@" >/dev/null 2>&1
  fi
}

# Acquire sudo. Returns 0 if root or sudo obtained.
# In quiet mode, skips interactive prompt and returns 1 silently.
_shdeps_require_sudo() {
  if [[ "$(id -u)" -eq 0 ]]; then return 0; fi
  if sudo -n true 2>/dev/null; then return 0; fi
  if [[ "$(_shdeps_quiet)" -eq 1 ]]; then return 1; fi
  sudo true
}

# Check if any pkg dep needs installing on this platform.
# Sets _SHDEPS_PKG_INSTALL_NEEDED=1 if so. Idempotent — only scans once.
# Call after _shdeps_load and _shdeps_pkg_detect.
_shdeps_check_pkg_needed() {
  [[ "${_SHDEPS_PKG_INSTALL_NEEDED:-}" != 0 ]] || return 0
  [[ "${_SHDEPS_PKG_INSTALL_NEEDED:-}" != 1 ]] || return 0
  _SHDEPS_PKG_INSTALL_NEEDED=0

  if [[ "$_SHDEPS_PKG_MGR" == "brew" || -z "$_SHDEPS_PKG_MGR" ]]; then return 0; fi

  local entry
  for entry in "${_SHDEPS_DEPS[@]}"; do
    _shdeps_parse "$entry"
    [[ "$_method" == "pkg" ]] || continue
    _shdeps_platform_match "$_platforms" || continue
    _shdeps_host_match "$_hosts" || continue
    local resolved_pkg
    resolved_pkg=$(_shdeps_pkg_resolve "$_name" "$_source")
    [[ "$resolved_pkg" == "NONE" ]] && continue
    if ! _shdeps_exists "$_cmd" "$_cmd_alt" "$resolved_pkg"; then
      _SHDEPS_PKG_INSTALL_NEEDED=1
      return 0
    fi
  done
}

# Prime sudo credentials if any deps will need them.
# Call after _shdeps_load and _shdeps_pkg_detect.
_shdeps_maybe_prime_sudo() {
  _shdeps_check_pkg_needed
  [[ "${_SHDEPS_PKG_INSTALL_NEEDED:-0}" -eq 1 ]] || return 0
  if [[ "$(id -u)" -eq 0 ]]; then return 0; fi
  if sudo -n true 2>/dev/null; then return 0; fi
  if [[ "$(_shdeps_quiet)" -eq 1 ]]; then return 0; fi
  _shdeps_log "  sudo required for package installs"
  _shdeps_require_sudo
}

# ---------------------------------------------------------------------------
# Config parsing
# ---------------------------------------------------------------------------

# Parse config files into _SHDEPS_DEPS array.
# Loads all *.conf files from SHDEPS_CONF_DIR (sorted alphabetically).
# Each non-blank, non-comment line becomes a pipe-delimited entry via
# word splitting.
_shdeps_load() {
  _SHDEPS_DEPS=()
  local conf_dir
  conf_dir=$(_shdeps_conf_dir)

  # Collect sorted *.conf files
  local -a conf_files=()
  local f
  if [[ -d "$conf_dir" ]]; then
    while IFS= read -r -d '' f; do
      conf_files+=("$f")
    done < <(find "$conf_dir" -maxdepth 1 -name '*.conf' -print0 2>/dev/null | LC_ALL=C sort -z)
  fi

  if [[ ${#conf_files[@]} -eq 0 ]]; then
    _shdeps_warn "  warning: no *.conf files in $conf_dir — skipping dependency install"
    return 0
  fi

  local line
  for f in "${conf_files[@]}"; do
    while IFS= read -r line || [[ -n "$line" ]]; do
      # Skip comments and blank lines
      if [[ "$line" =~ ^[[:space:]]*# ]]; then continue; fi
      if [[ -z "${line// /}" ]]; then continue; fi
      # Word-split the line, rejoin with pipe delimiters
      local fields
      # shellcheck disable=SC2086  # intentional word splitting
      set -- $line
      fields="$*"
      _SHDEPS_DEPS+=("${fields// /|}")
    done <"$f"
  done

  # Sort alphabetically by name for stable, scannable output.
  # Processing order doesn't affect correctness — pkg deps are batched,
  # and git/binary/custom deps are independent of each other.
  if [[ ${#_SHDEPS_DEPS[@]} -gt 1 ]]; then
    local -a sorted=()
    readarray -t sorted < <(printf '%s\n' "${_SHDEPS_DEPS[@]}" | LC_ALL=C sort -t'|' -k1,1f)
    _SHDEPS_DEPS=("${sorted[@]}")
  fi
}

# ---------------------------------------------------------------------------
# Entry parsing — split pipe-delimited entries into named variables
# ---------------------------------------------------------------------------

# Split a pipe-delimited registry entry into named variables.
# Sets: _name, _method, _cmd, _cmd_alt, _source, _platforms, _hosts
# _source is method-dependent: pkg_overrides for pkg, owner/repo for git/binary.
_shdeps_parse() {
  local entry="$1"
  IFS='|' read -r _name _method _cmd _cmd_alt _source _platforms _hosts <<<"$entry"
  # Dash means "use default" / "not specified"
  if [[ "$_cmd" == "-" ]]; then _cmd=""; fi
  if [[ "$_cmd_alt" == "-" ]]; then _cmd_alt=""; fi
  if [[ "$_source" == "-" ]]; then _source=""; fi
  if [[ "$_platforms" == "-" ]]; then _platforms=""; fi
  if [[ "$_hosts" == "-" ]]; then _hosts=""; fi
  # Default cmd to name when unspecified
  if [[ -z "$_cmd" ]]; then _cmd="$_name"; fi
}

# ---------------------------------------------------------------------------
# Platform matching — include/exclude filter on OS
# ---------------------------------------------------------------------------

# Check if the current platform matches a platforms spec.
# Empty spec matches all platforms. Supports include (linux,macos)
# and exclude (!wsl,!macos) lists. Mixed lists check excludes first.
# Returns 0 if the dep should install on this platform.
_shdeps_platform_match() {
  local spec="${1:-}"
  if [[ -z "$spec" ]]; then return 0; fi

  local current
  current=$(shdeps_platform)

  local item has_include=0 has_exclude=0
  local IFS=','
  for item in $spec; do
    if [[ "$item" == !* ]]; then has_exclude=1; else has_include=1; fi
  done

  if [[ $has_include -eq 1 && $has_exclude -eq 1 ]]; then
    # Mixed: excludes take priority, then check includes
    for item in $spec; do
      if [[ "$item" == "!$current" ]]; then return 1; fi
    done
    for item in $spec; do
      if [[ "$item" == "$current" ]]; then return 0; fi
    done
    return 1
  elif [[ $has_exclude -eq 1 ]]; then
    # Exclude-only: match unless explicitly excluded
    for item in $spec; do
      if [[ "$item" == "!$current" ]]; then return 1; fi
    done
    return 0
  else
    # Include-only: match only if listed
    for item in $spec; do
      if [[ "$item" == "$current" ]]; then return 0; fi
    done
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Host matching — include/exclude filter on hostname
# ---------------------------------------------------------------------------

# Check if the current hostname matches a hosts spec.
# Same logic as _shdeps_platform_match but compares against hostname.
# Empty spec matches all hosts. Supports include (nas,taylor) and
# exclude (!nas) lists. Mixed lists check excludes first.
# Returns 0 if the dep should install on this host.
_shdeps_host_match() {
  local spec="${1:-}"
  if [[ -z "$spec" ]]; then return 0; fi

  local current
  current=$(hostname -s 2>/dev/null || hostname 2>/dev/null)
  current="${current,,}"

  local item has_include=0 has_exclude=0
  local IFS=','
  for item in $spec; do
    item="${item,,}"
    if [[ "$item" == !* ]]; then has_exclude=1; else has_include=1; fi
  done

  if [[ $has_include -eq 1 && $has_exclude -eq 1 ]]; then
    for item in $spec; do
      item="${item,,}"
      if [[ "$item" == "!$current" ]]; then return 1; fi
    done
    for item in $spec; do
      item="${item,,}"
      if [[ "$item" == "$current" ]]; then return 0; fi
    done
    return 1
  elif [[ $has_exclude -eq 1 ]]; then
    for item in $spec; do
      item="${item,,}"
      if [[ "$item" == "!$current" ]]; then return 1; fi
    done
    return 0
  else
    for item in $spec; do
      item="${item,,}"
      if [[ "$item" == "$current" ]]; then return 0; fi
    done
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Dep existence and version checking
# ---------------------------------------------------------------------------

# Check if a dependency is installed. Returns 0 if cmd or cmd_alt is found.
# Falls back to querying the package manager when command lookup fails
# (useful for deps like fonts that don't provide binaries).
_shdeps_exists() {
  local cmd="${1:-}" alt="${2:-}" name="${3:-}"
  if [[ -n "$cmd" ]]; then
    if command -v "$cmd" &>/dev/null; then return 0; fi
    if [[ -n "$alt" ]]; then
      if command -v "$alt" &>/dev/null; then return 0; fi
    fi
  fi
  # Command not found (or empty) — try the package manager directly.
  if [[ -n "$name" ]]; then
    case "${_SHDEPS_PKG_MGR:-}" in
    brew)   brew list "$name" &>/dev/null && return 0 ;;
    apt)    dpkg -s "$name" &>/dev/null && return 0 ;;
    dnf)    rpm -q "$name" &>/dev/null && return 0 ;;
    pacman) pacman -Q "$name" &>/dev/null && return 0 ;;
    esac
  fi
  return 1
}

# Get installed version of a command.
# Extracts the first version-like token (digits+dots) from --version output.
_shdeps_dep_version() {
  local cmd="${1:-}" output="" ver="" all_output=""
  if [[ -z "$cmd" ]]; then return 1; fi
  # Try --version then -V (tmux, autossh, ssh). Skip -v (grep -v inverts
  # match, gzip -v compresses stdin) and bare "version" subcommand (curl
  # fetches http://version, ssh connects to host "version").
  # Merge stderr — some tools (unzip, ssh) write version info there.
  # Search all output lines — some (eza, shellcheck) put it on line 2+.
  # Timeout after 2s — some tools (nano/pico on macOS) hang with --version.
  #
  # Two passes: first try dotted versions (1.2.3, 3.6a) across all flags,
  # then fall back to integer-only versions anchored near the tool name
  # or "version" keyword (less 668, gzip 479). Two passes prevent the
  # fallback from matching noise in --version error output when -V has
  # the real answer (e.g., ssh).
  local flag _timeout=""
  command -v timeout &>/dev/null && _timeout="timeout 2"
  for flag in --version -V; do
    # shellcheck disable=SC2086  # intentional word splitting on $_timeout
    output=$($_timeout "$cmd" "$flag" 2>&1 || true)
    ver=$(echo "$output" | grep -oE '[0-9]+\.[0-9]+[0-9.a-z]*' | head -1)
    if [[ -n "$ver" ]]; then
      echo "$ver"
      return 0
    fi
    all_output+="$output"$'\n'
  done
  # Fallback: integer-only version near the tool name or "version" keyword.
  local name="${cmd##*/}"
  ver=$(echo "$all_output" |
    grep -iE "(version|$name)" |
    grep -oE '[0-9]+[a-z]?' | head -1)
  if [[ -n "$ver" ]]; then
    echo "$ver"
  fi
}

# ---------------------------------------------------------------------------
# Package manager abstraction
# ---------------------------------------------------------------------------

# Detect available package manager. Sets _SHDEPS_PKG_MGR.
_shdeps_pkg_detect() {
  if [[ "$(uname -s 2>/dev/null)" == "Darwin" ]] && command -v brew &>/dev/null; then
    _SHDEPS_PKG_MGR="brew"
  elif command -v apt-get &>/dev/null; then
    _SHDEPS_PKG_MGR="apt"
  elif command -v dnf &>/dev/null; then
    _SHDEPS_PKG_MGR="dnf"
  elif command -v pacman &>/dev/null; then
    _SHDEPS_PKG_MGR="pacman"
  else
    _SHDEPS_PKG_MGR=""
  fi
}

# Resolve canonical package name to OS-specific name via overrides.
# $1=name $2=source (pkg overrides, e.g. "apt:fd-find,dnf:fd-find")
_shdeps_pkg_resolve() {
  local name="$1" overrides="${2:-}"
  if [[ -n "$overrides" && -n "$_SHDEPS_PKG_MGR" ]]; then
    local pair pairs
    IFS=',' read -ra pairs <<<"$overrides"
    for pair in "${pairs[@]}"; do
      local mgr="${pair%%:*}"
      local pkg="${pair#*:}"
      if [[ "$mgr" == "$_SHDEPS_PKG_MGR" ]]; then
        echo "$pkg"
        return 0
      fi
    done
  fi
  echo "$name"
}

# Check if a package is available in the current package manager's repos.
_shdeps_pkg_available() {
  local pkg="$1"
  case "$_SHDEPS_PKG_MGR" in
  brew)   brew info "$pkg" &>/dev/null ;;
  apt)    apt-cache show "$pkg" &>/dev/null ;;
  dnf)    dnf info "$pkg" &>/dev/null ;;
  pacman) pacman -Si "$pkg" &>/dev/null ;;
  *)      return 0 ;;
  esac
}

# Refresh package manager metadata cache once per update run.
# Ensures _shdeps_pkg_available sees all available packages, even on
# a fresh machine where the cache is empty.
_shdeps_pkg_refresh_metadata() {
  _shdeps_check_pkg_needed
  [[ "${_SHDEPS_PKG_INSTALL_NEEDED:-0}" -eq 1 ]] || return 0
  [[ "${_SHDEPS_PKG_CACHE_REFRESHED:-}" != 1 ]] || return 0

  # apt/dnf/pacman write to root-owned cache dirs — require sudo.
  if [[ "$(id -u)" -ne 0 ]] && ! sudo -n true 2>/dev/null; then
    _SHDEPS_PKG_CACHE_REFRESHED=1
    return 0
  fi

  _shdeps_log_status "  refreshing $_SHDEPS_PKG_MGR package metadata..."
  case "$_SHDEPS_PKG_MGR" in
  apt)    sudo apt-get update -qq >/dev/null 2>&1 || true ;;
  dnf)    sudo dnf makecache -q &>/dev/null || true ;;
  pacman) sudo pacman -Sy &>/dev/null || true ;;
  esac
  _SHDEPS_PKG_CACHE_REFRESHED=1
  _shdeps_log_dim "  $_SHDEPS_PKG_MGR package metadata refreshed"
}

# Queue a package for batched install.
# $1=name $2=source (pkg overrides)
# Skips if resolved name is NONE (platform not supported) or unavailable.
_shdeps_pkg_queue() {
  local name="$1" overrides="${2:-}"
  local resolved
  resolved=$(_shdeps_pkg_resolve "$name" "$overrides")
  if [[ "$resolved" == "NONE" ]]; then
    return 0
  fi
  if ! _shdeps_pkg_available "$resolved"; then
    _shdeps_warn "  warning: $name not available in $_SHDEPS_PKG_MGR repos — skipping"
    return 0
  fi
  _SHDEPS_PKG_BATCH+=("$resolved")
  _SHDEPS_PKG_BATCH_NAMES+=("$name")
  _shdeps_log "  $name queued for install"
}

# Install all queued packages in a single command.
# On batch failure, retries each package individually.
_shdeps_pkg_install_batch() {
  if [[ ${#_SHDEPS_PKG_BATCH[@]} -eq 0 ]]; then return 0; fi

  if [[ -z "$_SHDEPS_PKG_MGR" ]]; then
    _shdeps_warn "  warning: no package manager found — cannot install: ${_SHDEPS_PKG_BATCH[*]}"
    return 0
  fi

  # Quiet mode: proceed if root, silently skip otherwise
  if [[ "$(_shdeps_quiet)" -eq 1 && "$(id -u)" -ne 0 ]]; then
    return 0
  fi

  _shdeps_log_ok "  installing: ${_SHDEPS_PKG_BATCH[*]}"

  # Non-brew managers need sudo
  if [[ "$_SHDEPS_PKG_MGR" != "brew" ]] && ! _shdeps_require_sudo; then
    _shdeps_warn "  warning: sudo not available — cannot install: ${_SHDEPS_PKG_BATCH[*]}"
    return 0
  fi

  local rc=0
  local log=""
  if ! _shdeps_logfile_create; then
    _shdeps_warn "  warning: failed to create temp log for package install"
  else
    log="$REPLY"
  fi

  # Batch install
  _shdeps_log_status "  ${_SHDEPS_PKG_MGR}: installing ${#_SHDEPS_PKG_BATCH[@]} packages..."
  # shellcheck disable=SC2024  # sudo output captured in user-owned log
  case "$_SHDEPS_PKG_MGR" in
  brew)   _shdeps_run_logged brew install "${_SHDEPS_PKG_BATCH[@]}" || rc=$? ;;
  apt)    _shdeps_run_logged sudo apt-get install -y "${_SHDEPS_PKG_BATCH[@]}" || rc=$? ;;
  dnf)    _shdeps_run_logged sudo dnf install -y "${_SHDEPS_PKG_BATCH[@]}" || rc=$? ;;
  pacman) _shdeps_run_logged sudo pacman -Sy --needed --noconfirm "${_SHDEPS_PKG_BATCH[@]}" || rc=$? ;;
  esac
  _shdeps_log_clear

  # On batch failure, retry individually so one bad package doesn't block all
  if [[ $rc -ne 0 ]]; then
    _shdeps_logfile_print "package manager" "$log"
    _shdeps_warn "  warning: batch install failed, retrying individually..."
    local pkg
    for pkg in "${_SHDEPS_PKG_BATCH[@]}"; do
      rc=0
      if [[ -n "$log" ]]; then : >"$log"; fi
      _shdeps_log_status "  ${_SHDEPS_PKG_MGR}: installing $pkg..."
      # shellcheck disable=SC2024  # sudo output captured in user-owned log
      case "$_SHDEPS_PKG_MGR" in
      brew)   _shdeps_run_logged brew install "$pkg" || rc=$? ;;
      apt)    _shdeps_run_logged sudo apt-get install -y "$pkg" || rc=$? ;;
      dnf)    _shdeps_run_logged sudo dnf install -y "$pkg" || rc=$? ;;
      pacman) _shdeps_run_logged sudo pacman -Sy --needed --noconfirm "$pkg" || rc=$? ;;
      esac
      if [[ $rc -ne 0 ]]; then
        _shdeps_logfile_print "package manager for $pkg" "$log"
        _shdeps_warn "  warning: failed to install $pkg"
        rc=0
      fi
    done
  fi

  rm -f "$log"

  # Mark all batch-installed deps as changed
  local _i
  for _i in "${!_SHDEPS_PKG_BATCH_NAMES[@]}"; do
    _SHDEPS_CHANGED[${_SHDEPS_PKG_BATCH_NAMES[$_i]}]=1
  done
}

# ---------------------------------------------------------------------------
# Remote-check cache — TTL-based stamps to avoid redundant network calls
# ---------------------------------------------------------------------------

# Return path for a dep's cache stamp file.
# $1=name $2=kind (git, binary, etc.)
_shdeps_remote_stamp() {
  local name="$1" kind="$2"
  echo "$(_shdeps_state_dir)/${name}.${kind}.stamp"
}

# Check if a stamp is still fresh (within TTL). Returns 0 if fresh.
# Force or reinstall mode always returns 1 (stale).
_shdeps_remote_fresh() {
  local stamp="$1"
  if [[ "$(_shdeps_force)" -eq 1 || "$(_shdeps_reinstall)" -eq 1 ]]; then return 1; fi
  [[ -f "$stamp" ]] || return 1

  local cached="" now="" ttl=""
  read -r cached <"$stamp" || return 1
  now=$(date +%s 2>/dev/null || true)
  ttl=$(_shdeps_remote_ttl)

  [[ "$cached" =~ ^[0-9]+$ ]] || return 1
  [[ "$now" =~ ^[0-9]+$ ]] || return 1
  [[ "$ttl" =~ ^[0-9]+$ ]] || return 1

  ((now - cached < ttl))
}

# Touch (update) a stamp with the current epoch time.
_shdeps_remote_touch() {
  local stamp="$1"
  local stamp_dir
  stamp_dir=$(dirname "$stamp")
  mkdir -p "$stamp_dir" || return 1
  date +%s >"$stamp"
}

# Return path for a dep's git revision stamp.
_shdeps_rev_stamp() {
  local name="$1"
  echo "$(_shdeps_state_dir)/${name}.rev"
}

# Read the cached git revision for a dep. Sets REPLY.
_shdeps_rev_read() {
  local name="$1"
  local stamp
  stamp=$(_shdeps_rev_stamp "$name")
  [[ -f "$stamp" ]] || return 1
  read -r REPLY <"$stamp" || return 1
}

# Write a git revision to the dep's rev stamp.
_shdeps_rev_touch() {
  local name="$1" rev="$2"
  local stamp
  stamp=$(_shdeps_rev_stamp "$name")
  local stamp_dir
  stamp_dir=$(dirname "$stamp")
  mkdir -p "$stamp_dir" || return 1
  printf '%s\n' "$rev" >"$stamp"
}

# ---------------------------------------------------------------------------
# Manifest — tracks installed deps for orphan detection and pruning
# ---------------------------------------------------------------------------

# Return path to the manifest file.
_shdeps_manifest_path() {
  echo "$(_shdeps_state_dir)/manifest"
}

# Read the manifest file into _SHDEPS_MANIFEST associative array.
# Each entry: _SHDEPS_MANIFEST[$name]="method|cmd|install_path"
_shdeps_manifest_read() {
  declare -gA _SHDEPS_MANIFEST=()
  local manifest
  manifest=$(_shdeps_manifest_path)
  [[ -f "$manifest" ]] || return 0
  local m_name m_rest
  while IFS='|' read -r m_name m_rest || [[ -n "$m_name" ]]; do
    [[ -n "$m_name" ]] || continue
    _SHDEPS_MANIFEST[$m_name]="$m_rest"
  done <"$manifest"
}

# Add or update a manifest entry.
# $1=name $2=method $3=cmd $4=install_path
_shdeps_manifest_upsert() {
  local name="$1" method="$2" cmd="$3" install_path="${4:-}"
  local manifest
  manifest=$(_shdeps_manifest_path)
  local manifest_dir
  manifest_dir=$(dirname "$manifest")
  mkdir -p "$manifest_dir" || return 1
  # Remove existing entry (grep -v to temp file + mv for portability)
  if [[ -f "$manifest" ]]; then
    local tmp="$manifest.tmp.$$"
    grep -v "^${name}|" "$manifest" >"$tmp" 2>/dev/null || true
    mv "$tmp" "$manifest"
  fi
  printf '%s\n' "$name|$method|$cmd|$install_path" >>"$manifest"
}

# Remove a manifest entry by name.
_shdeps_manifest_remove() {
  local name="$1"
  local manifest
  manifest=$(_shdeps_manifest_path)
  [[ -f "$manifest" ]] || return 0
  local tmp="$manifest.tmp.$$"
  grep -v "^${name}|" "$manifest" >"$tmp" 2>/dev/null || true
  mv "$tmp" "$manifest"
}

# Detect orphaned deps: in manifest but not in config.
# Sets _SHDEPS_ORPHANS array with "name|method|cmd|install_path" entries.
# Returns 0 if orphans found, 1 if none.
_shdeps_detect_orphans() {
  _SHDEPS_ORPHANS=()
  _shdeps_manifest_read

  if [[ ${#_SHDEPS_MANIFEST[@]} -eq 0 ]]; then return 1; fi

  # Build set of all config dep names (regardless of platform/host filtering)
  local -A config_names=()
  local entry
  for entry in "${_SHDEPS_DEPS[@]}"; do
    config_names["${entry%%|*}"]=1
  done

  # Any manifest entry not in config is an orphan
  local name
  for name in "${!_SHDEPS_MANIFEST[@]}"; do
    if [[ -z "${config_names[$name]+x}" ]]; then
      _SHDEPS_ORPHANS+=("$name|${_SHDEPS_MANIFEST[$name]}")
    fi
  done

  [[ ${#_SHDEPS_ORPHANS[@]} -gt 0 ]]
}

# Remove state stamps for a dep.
_shdeps_remove_stamps() {
  local name="$1"
  local state_dir
  state_dir=$(_shdeps_state_dir)
  rm -f "$state_dir/$name".*.stamp "$state_dir/$name.rev"
}

# Remove artifacts for one orphaned dep.
# Calls uninstall() from hook file if defined (all methods), then does
# built-in cleanup per method.
# $1=name $2=method $3=cmd $4=install_path
_shdeps_remove_dep() {
  local name="$1" method="$2" cmd="$3" install_path="$4"

  # Run uninstall() hook if present (reverses what post()/install() created)
  local hooks_dir hook_file _had_uninstall=0
  hooks_dir=$(_shdeps_hooks_dir)
  hook_file="$hooks_dir/$name.sh"
  if [[ -f "$hook_file" ]]; then
    unset -f exists version install post uninstall 2>/dev/null
    # shellcheck source=/dev/null
    if ! . "$hook_file"; then
      _shdeps_warn "  warning: failed to source $hook_file"
    elif declare -f uninstall &>/dev/null; then
      _had_uninstall=1
      if ! uninstall "$name"; then
        _shdeps_warn "  warning: $name uninstall hook failed"
      fi
    fi
    unset -f exists version install post uninstall 2>/dev/null
  fi

  # Built-in cleanup per method
  case "$method" in
  pkg)
    _shdeps_warn "  $name: pkg dep — remove manually via ${_SHDEPS_PKG_MGR:-system package manager}"
    ;;
  git)
    if [[ -n "$install_path" ]]; then
      # install_path may be absolute (current format) or relative to HOME (legacy manifest entries)
      local rm_path="$install_path"
      [[ "$rm_path" != /* ]] && rm_path="${HOME:?}/$rm_path"
      rm -rf "$rm_path"
    fi
    rm -f "$(_shdeps_bin_dir)/$name"
    _shdeps_remove_stamps "$name"
    _shdeps_log_ok "  $name removed"
    ;;
  binary)
    rm -f "$(_shdeps_bin_dir)/$cmd"
    local binary_install_dir
    binary_install_dir="$(_shdeps_install_dir)/$name"
    if [[ -d "$binary_install_dir" ]]; then
      rm -rf "$binary_install_dir"
    fi
    _shdeps_remove_stamps "$name"
    _shdeps_log_ok "  $name removed"
    ;;
  custom)
    if [[ $_had_uninstall -eq 1 ]]; then
      _shdeps_log_ok "  $name removed"
    elif [[ ! -f "$hook_file" ]]; then
      _shdeps_warn "  warning: $name hook file missing — manual cleanup may be needed"
    else
      _shdeps_warn "  warning: $name has no uninstall() hook — manual cleanup may be needed"
    fi
    _shdeps_remove_stamps "$name"
    ;;
  esac
}

# Remove all orphaned deps (in manifest but not in config).
# Flags: -y (skip confirmation), --dry-run (show what would be removed).
_shdeps_prune() {
  local yes=0 dry_run=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
    -y)        yes=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    *)         _shdeps_warn "error: unknown prune option '$1'"; return 2 ;;
    esac
  done

  _shdeps_load
  _shdeps_pkg_detect

  # Guard: if config is empty but manifest has entries, all deps would appear
  # orphaned. Require explicit -y to proceed (prevents accidental wipe from
  # a missing or empty config dir).
  if [[ ${#_SHDEPS_DEPS[@]} -eq 0 ]]; then
    _shdeps_manifest_read
    if [[ ${#_SHDEPS_MANIFEST[@]} -gt 0 && $yes -eq 0 ]]; then
      _shdeps_warn "warning: no deps in config but ${#_SHDEPS_MANIFEST[@]} in manifest — all would be orphaned"
      _shdeps_warn "  If intentional, re-run with -y"
      return 1
    fi
  fi

  if ! _shdeps_detect_orphans; then
    _shdeps_log "No orphaned deps found."
    return 0
  fi

  # List orphans
  local count=${#_SHDEPS_ORPHANS[@]}
  _shdeps_log_header "==> $count orphaned dep(s) no longer in config:"
  local orphan o_name o_method o_cmd o_install_path
  for orphan in "${_SHDEPS_ORPHANS[@]}"; do
    IFS='|' read -r o_name o_method o_cmd o_install_path <<<"$orphan"
    _shdeps_log "  $o_name ($o_method)"
  done

  if [[ $dry_run -eq 1 ]]; then
    _shdeps_log "Dry run — nothing removed."
    return 0
  fi

  # Quiet mode without -y: silently skip (no prompt, no action)
  if [[ "$(_shdeps_quiet)" -eq 1 && $yes -eq 0 ]]; then
    return 0
  fi

  # Confirm unless -y
  if [[ $yes -eq 0 ]]; then
    local reply=""
    read -r -p "Remove? [y/N] " reply
    if [[ "${reply,,}" != "y" ]]; then
      _shdeps_log "Aborted."
      return 0
    fi
  fi

  # Remove each orphan
  for orphan in "${_SHDEPS_ORPHANS[@]}"; do
    IFS='|' read -r o_name o_method o_cmd o_install_path <<<"$orphan"
    _shdeps_remove_dep "$o_name" "$o_method" "$o_cmd" "$o_install_path" || true
    _shdeps_manifest_remove "$o_name"
  done
}

# ---------------------------------------------------------------------------
# GitHub/git install methods
# ---------------------------------------------------------------------------

# Get version string for an installed tool.
# Checks: VERSION file, git describe, git log hash.
_shdeps_get_version() {
  local dir="$1"
  if [[ -f "$dir/VERSION" ]]; then
    echo "v$(cat "$dir/VERSION")"
  elif [[ -d "$dir/.git" ]]; then
    local ver
    ver=$(git -C "$dir" describe --tags --abbrev=0 2>/dev/null || true)
    if [[ -z "$ver" ]]; then
      local hash
      hash=$(git -C "$dir" log -1 --format='%h' 2>/dev/null || true)
      if [[ -n "$hash" ]]; then ver="commit $hash"; fi
    fi
    echo "$ver"
  fi
}

# Symlink bin/<name> into $SHDEPS_BIN_DIR if it exists.
_shdeps_link_bin() {
  local name="$1" install_dir="$2"
  if [[ -x "$install_dir/bin/$name" ]]; then
    mkdir -p "$(_shdeps_bin_dir)"
    ln -sf "$install_dir/bin/$name" "$(_shdeps_bin_dir)/$name"
  fi
}

# Strategy: ~/git/<name> exists — symlink for live development, pull if TTL
# expired and the working tree is clean.
_shdeps_github_install_local_clone() {
  local name="$1" local_clone="$2" install_dir="$3" stamp="$4" log="$5"
  local link_before=""
  if [[ -L "$install_dir" ]]; then
    link_before=$(readlink "$install_dir" 2>/dev/null || true)
  fi

  local rev_before="" rev_after="" dirty_after=0
  if _shdeps_rev_read "$name"; then
    rev_before="$REPLY"
  fi

  # Check worktree status once — reused for both pull gating and dirty flag.
  local _status_output=""
  _status_output=$(git -C "$local_clone" status --porcelain --untracked-files=normal 2>/dev/null || true)

  # Pull if TTL expired (or --force) and the clone is clean
  if ! _shdeps_remote_fresh "$stamp"; then
    if [[ -z "$_status_output" ]]; then
      _shdeps_log_status "  $name: pulling latest..."
      if _shdeps_run_logged git -C "$local_clone" pull --ff-only --quiet; then
        _shdeps_remote_touch "$stamp" || true
      else
        _shdeps_logfile_print "$name update" "$log"
        _shdeps_warn "  warning: $name update failed"
      fi
      # Re-check status only if pull ran (it may have introduced changes)
      _status_output=$(git -C "$local_clone" status --porcelain --untracked-files=normal 2>/dev/null || true)
    fi
  fi

  rev_after=$(git -C "$local_clone" rev-parse HEAD 2>/dev/null || true)
  if [[ -n "$_status_output" ]]; then
    dirty_after=1
  fi

  rm -rf "$install_dir"
  mkdir -p "$(dirname "$install_dir")"
  ln -sfn "$local_clone" "$install_dir"
  _shdeps_link_bin "$name" "$install_dir"

  local ver
  ver=$(_shdeps_get_version "$local_clone")
  if [[ -n "$rev_after" ]]; then
    _shdeps_rev_touch "$name" "$rev_after" || true
  fi

  if [[ "$link_before" != "$local_clone" ]]; then
    _SHDEPS_CHANGED[$name]=1
    _shdeps_log_ok "  $name added${ver:+ -- $ver} (local clone)"
  elif [[ "$rev_before" != "$rev_after" ]]; then
    _SHDEPS_CHANGED[$name]=1
    _shdeps_log_ok "  $name updated${ver:+ -- $ver} (local clone)"
  elif [[ "$dirty_after" -eq 1 || "$(_shdeps_reinstall)" -eq 1 ]]; then
    _SHDEPS_CHANGED[$name]=1
    _shdeps_log_ok "  $name reinstalled${ver:+ -- $ver} (local clone)"
  else
    _shdeps_log "  $name${ver:+ -- $ver} (local clone)"
  fi
  rm -f "$log"
}

# Strategy: install_dir/.git exists — pull to update.
_shdeps_github_install_pull() {
  local name="$1" install_dir="$2" stamp="$3" log="$4"
  if _shdeps_remote_fresh "$stamp"; then
    _shdeps_link_bin "$name" "$install_dir"
    local ver
    ver=$(_shdeps_get_version "$install_dir")
    _shdeps_log "  $name${ver:+ -- $ver}"
    rm -f "$log"
    return 0
  fi

  local head_before
  head_before=$(git -C "$install_dir" rev-parse HEAD 2>/dev/null || true)
  _shdeps_log_status "  $name: pulling latest..."
  if _shdeps_run_logged git -C "$install_dir" pull --ff-only --quiet; then
    _shdeps_link_bin "$name" "$install_dir"
    local head_after
    head_after=$(git -C "$install_dir" rev-parse HEAD 2>/dev/null || true)
    local ver
    ver=$(_shdeps_get_version "$install_dir")
    _shdeps_remote_touch "$stamp" || true
    if [[ "$head_before" != "$head_after" ]]; then
      _SHDEPS_CHANGED[$name]=1
      _shdeps_log_ok "  $name updated${ver:+ -- $ver}"
    elif [[ "$(_shdeps_reinstall)" -eq 1 ]]; then
      _SHDEPS_CHANGED[$name]=1
      _shdeps_log_ok "  $name reinstalled${ver:+ -- $ver}"
    else
      _shdeps_log "  $name${ver:+ -- $ver}"
    fi
  else
    _shdeps_logfile_print "$name update" "$log"
    _shdeps_warn "  warning: $name update failed"
  fi
  rm -f "$log"
}

# Strategy: no existing install — try release tarball, fall back to git clone.
_shdeps_github_install_fresh() {
  local name="$1" repo="$2" install_dir="$3" stamp="$4" log="$5"
  local tarball_url="" tmp_dir

  local ver_before
  ver_before=$(_shdeps_get_version "$install_dir")

  # Try GitHub release tarball first (faster than full clone).
  # Strip auth to prevent stale tokens from causing 401 on public repos.
  local gh_repo=""
  if [[ "$repo" =~ github\.com[:/]([^/]+/[^/.]+) ]]; then
    gh_repo="${BASH_REMATCH[1]}"
  fi
  if [[ -n "$gh_repo" ]] && command -v curl &>/dev/null; then
    tarball_url=$(curl -fsSL --no-netrc -H "Authorization:" \
      "https://api.github.com/repos/$gh_repo/releases/latest" 2>/dev/null |
      grep -o '"browser_download_url":[[:space:]]*"[^"]*\.tar\.gz"' |
      head -1 | cut -d'"' -f4)
  fi

  if [[ -n "${tarball_url:-}" ]]; then
    tmp_dir=$(mktemp -d)
    _shdeps_log_status "  $name: downloading release tarball..."
    # shellcheck disable=SC2016  # single quotes intentional — inner script uses $1/$2
    if _shdeps_run_logged bash -c 'curl -fsSL "$1" | tar xz -C "$2"' _ "$tarball_url" "$tmp_dir"; then
      rm -rf "$install_dir"
      mkdir -p "$install_dir"
      # Tarball typically has a top-level dir; move contents up
      mv "$tmp_dir"/*/* "$install_dir/" 2>/dev/null || mv "$tmp_dir"/* "$install_dir/"
      rm -rf "$tmp_dir"
    else
      rm -rf "$tmp_dir"
      _shdeps_logfile_print "$name release download" "$log"
      _shdeps_warn "  warning: failed to download $name release (trying git clone)"
      tarball_url=""
    fi
  fi

  # Fallback: git clone to a temp dir so we don't destroy an existing
  # install on failure (e.g. network unreachable).
  if [[ -z "${tarball_url:-}" ]]; then
    if ! command -v git &>/dev/null; then
      rm -f "$log"
      _shdeps_warn "  warning: no curl release and no git — cannot install $name"
      return 1
    fi
    local clone_tmp="${install_dir}.tmp.$$"
    rm -rf "$clone_tmp"
    if [[ -n "${log:-}" ]]; then : >"$log"; fi
    _shdeps_log_status "  $name: cloning repository..."
    if ! _shdeps_run_logged git clone --depth 1 "$repo" "$clone_tmp"; then
      rm -rf "$clone_tmp"
      _shdeps_logfile_print "$name clone" "$log"
      rm -f "$log"
      _shdeps_warn "  warning: failed to clone $name (network unreachable?)"
      return 1
    fi
    rm -rf "$install_dir"
    mv "$clone_tmp" "$install_dir"
  fi

  _shdeps_link_bin "$name" "$install_dir"
  rm -f "$log"
  _shdeps_remote_touch "$stamp" || true
  local ver
  ver=$(_shdeps_get_version "$install_dir")
  local method="git clone"
  if [[ -n "${tarball_url:-}" ]]; then method="release tarball"; fi

  if [[ -n "$ver_before" && "$ver_before" == "$ver" ]] && [[ "$(_shdeps_reinstall)" -ne 1 ]]; then
    _shdeps_log "  $name${ver:+ -- $ver} ($method)"
  else
    _SHDEPS_CHANGED[$name]=1
    if [[ -n "$ver_before" && "$ver_before" == "$ver" ]]; then
      _shdeps_log_ok "  $name reinstalled${ver:+ -- $ver} ($method)"
    else
      _shdeps_log_ok "  $name added${ver:+ -- $ver} ($method)"
    fi
  fi
}

# Install or upgrade a tool from GitHub (git method).
# Priority: $SHDEPS_GIT_DEV_DIR/<name> (symlink) > existing clone (pull) > release tarball > fresh clone.
# Env var override: SHDEPS_<NAME>_REPO overrides the repo URL.
_shdeps_install_from_github() {
  local name="$1" default_repo="$2" install_dir="$3"
  local upper="${name^^}"
  upper="${upper//-/_}"
  local env_var="SHDEPS_${upper}_REPO"
  local repo="${!env_var:-https://github.com/$default_repo}"
  local local_clone
  local_clone="$(_shdeps_git_dev_dir)/$name"
  local stamp
  stamp=$(_shdeps_remote_stamp "$name" git)
  local log=""
  if ! _shdeps_logfile_create; then
    _shdeps_warn "  warning: failed to create temp log for $name install"
  else
    log="$REPLY"
  fi

  if [[ -d "$local_clone" ]]; then
    _shdeps_github_install_local_clone "$name" "$local_clone" "$install_dir" "$stamp" "$log"
    return $?
  fi

  if [[ -d "$install_dir/.git" ]]; then
    _shdeps_github_install_pull "$name" "$install_dir" "$stamp" "$log"
    return $?
  fi

  _shdeps_github_install_fresh "$name" "$repo" "$install_dir" "$stamp" "$log"
}

# ---------------------------------------------------------------------------
# GitHub binary install methods
# ---------------------------------------------------------------------------

# Find a release asset URL matching the current OS and architecture.
# Multi-pass: standalone binary → tarball → zip.
# Prefers exact cmd-name matches and matching libc (gnu/musl).
# Prints the URL to stdout; empty string if no match.
_shdeps_binary_find_asset() {
  local cmd="$1" gh_repo="$2" tag="$3" release_json="$4"

  local os arch libc
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)

  # Detect system libc for preferring matching assets
  libc="gnu"
  if command -v ldd &>/dev/null && ldd --version 2>&1 | grep -qi musl; then
    libc="musl"
  fi

  # Normalize OS names (projects use various conventions)
  local os_patterns=("$os")
  case "$os" in
  darwin) os_patterns+=(macos apple osx) ;;
  linux)  os_patterns+=(linux) ;;
  esac

  # Normalize arch names
  local arch_patterns=("$arch")
  case "$arch" in
  x86_64)  arch_patterns+=(amd64 x64) ;;
  aarch64) arch_patterns+=(arm64) ;;
  amd64)   arch_patterns+=(x86_64 x64) ;;
  arm64)   arch_patterns+=(aarch64) ;;
  esac

  # Extensions to always skip (metadata, packages, installers)
  local -a _skip_exts=(
    .sha256 .sha512 .md5 .sig .asc .txt .json .zsync
    .sigstore .proof .sbom .b3 .pem .dmg .pkg .apk
    .deb .rpm .msi .appimage .flatpak .mcpb
  )

  # Archive extensions recognized for extraction
  local -a _tar_exts=(.tar.gz .tar.xz .tar.bz2 .tgz)
  local -a _archive_exts=("${_tar_exts[@]}" .zip)

  # Try matching from the API asset list.
  # Pass 1: standalone binaries (no archives).
  # Pass 2: tar archives.
  # Pass 3: zip archives (last — tarballs preferred).
  if [[ -n "$release_json" ]]; then
    local urls
    urls=$(echo "$release_json" |
      grep -o '"browser_download_url":[[:space:]]*"[^"]*"' |
      cut -d'"' -f4)

    if [[ -n "$urls" ]]; then
      local url url_lower arch_pat os_pat ext skip os_match is_archive
      local pass cmd_lower="${cmd,,}" fname is_exact suffix tok
      for pass in plain tarball zip; do
        local _exact_fallback="" _other_match="" _other_fallback=""
        while IFS= read -r url; do
          url_lower="${url,,}"

          # Must match at least one OS pattern
          os_match=0
          for os_pat in "${os_patterns[@]}"; do
            [[ "$url_lower" == *"$os_pat"* ]] && { os_match=1; break; }
          done
          [[ $os_match -eq 1 ]] || continue

          # Skip metadata and package files
          skip=0
          for ext in "${_skip_exts[@]}"; do
            [[ "$url_lower" == *"$ext" ]] && { skip=1; break; }
          done
          if [[ $skip -eq 1 ]]; then continue; fi

          # Pass-specific filtering
          if [[ "$pass" == "plain" ]]; then
            is_archive=0
            for ext in "${_archive_exts[@]}"; do
              [[ "$url_lower" == *"$ext" ]] && { is_archive=1; break; }
            done
            [[ $is_archive -eq 0 ]] || continue
          elif [[ "$pass" == "tarball" ]]; then
            is_archive=0
            for ext in "${_tar_exts[@]}"; do
              [[ "$url_lower" == *"$ext" ]] && { is_archive=1; break; }
            done
            [[ $is_archive -eq 1 ]] || continue
          else
            [[ "$url_lower" == *.zip ]] || continue
          fi

          # Check if filename matches the cmd name exactly (not a longer
          # name that happens to start with cmd). Repos with multiple
          # binaries need this to pick the right one.
          fname="${url_lower##*/}"
          is_exact=0
          if [[ "$fname" == "${cmd_lower}" ]]; then
            is_exact=1
          elif [[ "$fname" == "${cmd_lower}"[-_.]* ]]; then
            suffix="${fname#"${cmd_lower}"}"
            suffix="${suffix#[-_.]}"
            # Exact if suffix starts with OS, arch, or version pattern
            for tok in "${os_patterns[@]}" "${arch_patterns[@]}"; do
              [[ "$suffix" == "$tok"* ]] && { is_exact=1; break; }
            done
            if [[ $is_exact -eq 0 && "$suffix" =~ ^v?[0-9] ]]; then is_exact=1; fi
          fi

          for arch_pat in "${arch_patterns[@]}"; do
            if [[ "$url_lower" == *"$arch_pat"* ]]; then
              if [[ "$os" == "linux" && "$url_lower" == *"$libc"* ]]; then
                # Best: exact cmd + preferred libc
                if [[ $is_exact -eq 1 ]]; then
                  echo "$url"
                  return 0
                fi
                if [[ -z "$_other_match" ]]; then _other_match="$url"; fi
              else
                if [[ $is_exact -eq 1 ]]; then
                  if [[ -z "$_exact_fallback" ]]; then _exact_fallback="$url"; fi
                else
                  if [[ -z "$_other_fallback" ]]; then _other_fallback="$url"; fi
                fi
              fi
              break
            fi
          done
        done <<<"$urls"

        local result="${_exact_fallback:-${_other_match:-${_other_fallback:-}}}"
        if [[ -n "$result" ]]; then
          echo "$result"
          return 0
        fi
      done
    fi
  fi

  # Fallback: try common URL patterns when API didn't help
  local base="https://github.com/$gh_repo/releases/download/$tag"
  local o a
  for o in "${os_patterns[@]}"; do
    for a in "${arch_patterns[@]}"; do
      local candidates=(
        "$base/$cmd.${o}-${a}"
        "$base/${cmd}-${o}-${a}"
        "$base/${cmd}_${o}_${a}"
      )
      local c
      for c in "${candidates[@]}"; do
        if curl -fsSL --no-netrc --head "$c" &>/dev/null; then
          echo "$c"
          return 0
        fi
      done
    done
  done
}

# Find and install a binary from an extracted archive directory.
# Searches for the binary by name patterns and installs it into
# $SHDEPS_INSTALL_DIR/<name> with a symlink in $SHDEPS_BIN_DIR.
# $1=name $2=cmd $3=extract_dir $4=orig_extract_dir $5=bin_path
_shdeps_binary_install_from_extracted() {
  local name="$1" cmd="$2" extract_dir="$3" orig_extract_dir="$4" bin_path="$5"

  # If the archive has a single top-level directory, descend into it
  local top_entries
  top_entries=$(ls "$extract_dir")
  if [[ $(echo "$top_entries" | wc -l) -eq 1 && -d "$extract_dir/$top_entries" ]]; then
    extract_dir="$extract_dir/$top_entries"
  fi

  # Find the binary: exact name → prefix match → sole compiled-binary fallback
  local found_bin=""
  local pattern
  for pattern in "$cmd" "$cmd-*" "${cmd}_*"; do
    while IFS= read -r -d '' f; do
      if [[ -x "$f" ]]; then
        found_bin="$f"
        break 2
      fi
    done < <(find "$extract_dir" -name "$pattern" -type f -print0 2>/dev/null)
  done
  if [[ -z "$found_bin" ]]; then
    # Last resort: sole compiled binary (filter out scripts via file(1))
    local -a binaries=()
    while IFS= read -r -d '' f; do
      if [[ -x "$f" ]] && file "$f" | grep -qiE 'ELF|Mach-O'; then binaries+=("$f"); fi
    done < <(find "$extract_dir" -type f -print0 2>/dev/null)
    if [[ ${#binaries[@]} -eq 1 ]]; then found_bin="${binaries[0]}"; fi
  fi
  if [[ -z "$found_bin" ]]; then
    rm -rf "$orig_extract_dir"
    _shdeps_warn "  warning: $cmd binary not found in $name archive"
    return 1
  fi

  # Move extracted contents to $SHDEPS_INSTALL_DIR/<name>
  local install_dir
  install_dir="$(_shdeps_install_dir)/$name"
  rm -rf "$install_dir"
  mkdir -p "$(dirname "$install_dir")"
  mv "$extract_dir" "$install_dir"
  if [[ "$orig_extract_dir" != "$extract_dir" ]]; then rm -rf "$orig_extract_dir"; fi

  # Symlink the binary into PATH
  local bin_rel="${found_bin#"$extract_dir/"}"
  ln -sf "$install_dir/$bin_rel" "$bin_path"
}

# Extract a tarball, find the binary, install to $SHDEPS_INSTALL_DIR/<name>.
# $1=name $2=cmd $3=tmp_file $4=bin_path $5=log
_shdeps_binary_install_tarball() {
  local name="$1" cmd="$2" tmp_file="$3" bin_path="$4" log="$5"
  local extract_dir
  extract_dir=$(mktemp -d) || {
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: failed to create extract dir for $name"
    return 1
  }
  if ! tar xf "$tmp_file" -C "$extract_dir" 2>/dev/null; then
    rm -rf "$extract_dir" "$tmp_file" "$log"
    _shdeps_warn "  warning: failed to extract $name tarball"
    return 1
  fi
  rm -f "$tmp_file"
  _shdeps_binary_install_from_extracted "$name" "$cmd" "$extract_dir" "$extract_dir" "$bin_path"
}

# Extract a zip, find the binary, install to $SHDEPS_INSTALL_DIR/<name>.
# $1=name $2=cmd $3=tmp_file $4=bin_path $5=log
_shdeps_binary_install_zip() {
  local name="$1" cmd="$2" tmp_file="$3" bin_path="$4" log="$5"
  if ! command -v unzip &>/dev/null; then
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: unzip not found — cannot install $name"
    return 1
  fi
  local extract_dir
  extract_dir=$(mktemp -d) || {
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: failed to create extract dir for $name"
    return 1
  }
  if ! unzip -qo "$tmp_file" -d "$extract_dir" 2>/dev/null; then
    rm -rf "$extract_dir" "$tmp_file" "$log"
    _shdeps_warn "  warning: failed to extract $name zip"
    return 1
  fi
  rm -f "$tmp_file"
  _shdeps_binary_install_from_extracted "$name" "$cmd" "$extract_dir" "$extract_dir" "$bin_path"
}

# Install or upgrade a tool via GitHub release binary.
# Searches release assets for an executable matching the current OS/arch.
# Handles tarballs, zips, compressed singles (.gz/.bz2/.zst), and raw binaries.
# Usage: _shdeps_install_binary <name> <cmd> <owner/repo>
_shdeps_install_binary() {
  local name="$1" cmd="$2" gh_repo="$3"
  local bin_path
  bin_path="$(_shdeps_bin_dir)/$cmd"
  local current_ver="" latest_ver=""
  local log=""
  local stamp
  stamp=$(_shdeps_remote_stamp "$name" binary)

  # Fast path: skip entirely if binary exists and TTL is fresh.
  # Avoids temp file creation, API calls, and asset matching.
  if [[ -x "$bin_path" ]] && _shdeps_remote_fresh "$stamp"; then
    current_ver=$(_shdeps_dep_version "$cmd")
    _shdeps_log "  $name -- $current_ver"
    return 0
  fi

  if ! _shdeps_logfile_create; then
    _shdeps_warn "  warning: failed to create temp log for $name install"
  else
    log="$REPLY"
  fi
  local tmp_file
  tmp_file=$(mktemp) || {
    rm -f "$log"
    _shdeps_warn "  warning: failed to create temp file for $name install"
    return 1
  }

  # Get installed version (only when TTL is stale or binary is new)
  if [[ -x "$bin_path" ]]; then
    current_ver=$(_shdeps_dep_version "$cmd")
  fi

  # Use prefetched release JSON if available, otherwise fetch now.
  # The prefetch runs all binary API calls in parallel before the
  # sequential install loop, saving ~200-500ms per dep.
  local release_json=""
  if [[ "${_SHDEPS_PREFETCH_READY:-}" == 1 && -n "${_SHDEPS_PREFETCHED[$name]+x}" ]]; then
    release_json="${_SHDEPS_PREFETCHED[$name]}"
  elif command -v curl &>/dev/null; then
    local -a _gh_curl_args=(curl -fsSL --no-netrc)
    if [[ -n "${_SHDEPS_GH_TOKEN:-}" ]]; then
      _gh_curl_args+=(-H "Authorization: token $_SHDEPS_GH_TOKEN")
    else
      _gh_curl_args+=(-H "Authorization:")
    fi
    release_json=$("${_gh_curl_args[@]}" \
      "https://api.github.com/repos/$gh_repo/releases/latest" 2>/dev/null || true)
  fi
  latest_ver=$(echo "$release_json" |
    grep -o '"tag_name":[[:space:]]*"[^"]*"' | cut -d'"' -f4)

  # Extract numeric portion for comparison (handles v1.2.3, rust-v1.2.3, etc.)
  local latest_ver_num=""
  latest_ver_num=$(echo "$latest_ver" | grep -oE '[0-9]+\.[0-9]+[0-9.]*' | head -1)

  # Skip if already at latest (unless reinstall mode)
  if [[ "$(_shdeps_reinstall)" -ne 1 && -n "$current_ver" && -n "$latest_ver_num" && "$current_ver" == "$latest_ver_num" ]]; then
    rm -f "$tmp_file" "$log"
    _shdeps_remote_touch "$stamp" || true
    _shdeps_log "  $name -- $current_ver"
    return 0
  fi

  if [[ -z "$latest_ver" ]]; then
    if [[ -n "$current_ver" ]]; then
      rm -f "$tmp_file" "$log"
      _shdeps_warn "  warning: $name $current_ver (couldn't check for updates)"
      return 0
    fi
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: couldn't determine latest $name version"
    return 1
  fi

  # Find the right asset URL for this platform
  local asset_url=""
  asset_url=$(_shdeps_binary_find_asset "$cmd" "$gh_repo" "$latest_ver" "$release_json")

  if [[ -z "$asset_url" ]]; then
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: no matching release asset for $name $latest_ver"
    return 1
  fi

  if [[ -n "${log:-}" ]]; then : >"$log"; fi
  _shdeps_log_status "  $name: downloading $latest_ver..."
  if ! _shdeps_run_logged curl -fsSL --no-netrc "$asset_url" -o "$tmp_file"; then
    _shdeps_logfile_print "$name download" "$log"
    rm -f "$tmp_file" "$log"
    _shdeps_warn "  warning: failed to download $name $latest_ver"
    return 1
  fi

  mkdir -p "$(_shdeps_bin_dir)"

  # Install based on asset type: archive, compressed single, or direct binary
  local asset_lower="${asset_url,,}"
  if [[ "$asset_lower" == *.tar.gz || "$asset_lower" == *.tar.xz || "$asset_lower" == *.tar.bz2 || "$asset_lower" == *.tgz ]]; then
    if ! _shdeps_binary_install_tarball "$name" "$cmd" "$tmp_file" "$bin_path" "$log"; then
      return 1
    fi
  elif [[ "$asset_lower" == *.zip ]]; then
    if ! _shdeps_binary_install_zip "$name" "$cmd" "$tmp_file" "$bin_path" "$log"; then
      return 1
    fi
  elif [[ "$asset_lower" == *.gz ]]; then
    if ! gzip -dc "$tmp_file" > "$bin_path" 2>/dev/null; then
      rm -f "$tmp_file" "$bin_path" "$log"
      _shdeps_warn "  warning: failed to decompress $name .gz"
      return 1
    fi
    rm -f "$tmp_file"
    chmod u+x "$bin_path"
  elif [[ "$asset_lower" == *.bz2 ]]; then
    if ! bzip2 -dc "$tmp_file" > "$bin_path" 2>/dev/null; then
      rm -f "$tmp_file" "$bin_path" "$log"
      _shdeps_warn "  warning: failed to decompress $name .bz2"
      return 1
    fi
    rm -f "$tmp_file"
    chmod u+x "$bin_path"
  elif [[ "$asset_lower" == *.zst ]]; then
    if ! command -v zstd &>/dev/null; then
      rm -f "$tmp_file" "$log"
      _shdeps_warn "  warning: zstd not found — cannot install $name"
      return 1
    fi
    if ! zstd -df "$tmp_file" -o "$bin_path" 2>/dev/null; then
      rm -f "$tmp_file" "$log"
      _shdeps_warn "  warning: failed to decompress $name .zst"
      return 1
    fi
    rm -f "$tmp_file"
    chmod u+x "$bin_path"
  else
    mv "$tmp_file" "$bin_path"
    chmod u+x "$bin_path"
  fi
  rm -f "$log"
  _shdeps_remote_touch "$stamp" || true

  _SHDEPS_CHANGED[$name]=1
  if [[ -z "$current_ver" ]]; then
    _shdeps_log_ok "  $name added -- $latest_ver"
  elif [[ "$current_ver" == "$latest_ver_num" ]]; then
    _shdeps_log_ok "  $name reinstalled -- $latest_ver"
  else
    _shdeps_log_ok "  $name updated -- $current_ver -> $latest_ver"
  fi
}

# ---------------------------------------------------------------------------
# Dispatcher and hooks
# ---------------------------------------------------------------------------

# Route a dep registry entry to the appropriate install method.
_shdeps_install_dep() {
  local entry="$1"
  _shdeps_parse "$entry"

  # Skip deps that don't match this platform or host
  _shdeps_platform_match "$_platforms" || return 0
  _shdeps_host_match "$_hosts" || return 0

  case "$_method" in
  pkg)
    local resolved_pkg=""
    resolved_pkg=$(_shdeps_pkg_resolve "$_name" "$_source")
    if [[ "$resolved_pkg" == "NONE" ]]; then return 0; fi
    if _shdeps_exists "$_cmd" "$_cmd_alt" "$resolved_pkg"; then
      local ver=""
      ver=$(_shdeps_dep_version "$_cmd" 2>/dev/null || true)
      if [[ -z "$ver" && -n "$_cmd_alt" ]]; then
        ver=$(_shdeps_dep_version "$_cmd_alt" 2>/dev/null || true)
      fi
      _shdeps_log "  $_name${ver:+ -- $ver}"
      # Package exists but expected command missing — trigger post hook
      if ! _shdeps_exists "$_cmd" "$_cmd_alt"; then
        _SHDEPS_CHANGED[$_name]=1
      fi
      _shdeps_manifest_upsert "$_name" "pkg" "$_cmd" ""
      return 0
    fi
    _shdeps_pkg_queue "$_name" "$_source"
    _shdeps_manifest_upsert "$_name" "pkg" "$_cmd" ""
    ;;
  git)
    local _git_install_dir
    _git_install_dir="$(_shdeps_install_dir)/$_name"
    _shdeps_install_from_github "$_name" "$_source" "$_git_install_dir"
    _shdeps_manifest_upsert "$_name" "git" "$_cmd" "$_git_install_dir"
    ;;
  binary)
    _shdeps_install_binary "$_name" "$_cmd" "$_source"
    _shdeps_manifest_upsert "$_name" "binary" "$_cmd" "$(_shdeps_bin_dir)/$_cmd"
    ;;
  custom)
    # Source hook file and use exists() to gate install().
    local hooks_dir
    hooks_dir=$(_shdeps_hooks_dir)
    local hook_file="$hooks_dir/$_name.sh"
    [[ -f "$hook_file" ]] || return 0
    unset -f exists version install post uninstall 2>/dev/null
    # shellcheck source=/dev/null
    . "$hook_file" || {
      _shdeps_warn "  warning: failed to source $hook_file"
      return 0
    }
    if ! declare -f exists &>/dev/null; then
      _shdeps_warn "  warning: $_name hook missing exists() — skipping"
      unset -f exists version install post uninstall 2>/dev/null
      return 0
    fi
    local _existed=0 ver=""
    exists "$_name" && _existed=1
    if [[ $_existed -eq 1 && "$(_shdeps_reinstall)" -ne 1 ]]; then
      if declare -f version &>/dev/null; then
        ver=$(version "$_name" 2>/dev/null) || ver=""
      fi
      _shdeps_log "  $_name${ver:+ -- $ver}"
      unset -f exists version install post uninstall 2>/dev/null
      _shdeps_manifest_upsert "$_name" "custom" "$_cmd" ""
      return 0
    fi
    if declare -f install &>/dev/null; then
      local action="added"
      [[ $_existed -eq 1 ]] && action="reinstalled"
      if install "$_name"; then
        _SHDEPS_CHANGED[$_name]=1
        if declare -f version &>/dev/null; then
          ver=$(version "$_name" 2>/dev/null) || ver=""
        fi
        _shdeps_log_ok "  $_name $action${ver:+ -- $ver}"
        _shdeps_manifest_upsert "$_name" "custom" "$_cmd" ""
      else
        _shdeps_warn "  warning: $_name install failed"
      fi
    fi
    unset -f exists version install post uninstall 2>/dev/null
    ;;
  esac
}

# Run post() hooks for changed deps (post-install setup).
# install() already ran during _shdeps_install_dep for custom deps.
_shdeps_run_post_hooks() {
  local hooks_dir
  hooks_dir=$(_shdeps_hooks_dir)

  if [[ ${#_SHDEPS_CHANGED[@]} -eq 0 ]]; then return 0; fi

  local entry
  for entry in "${_SHDEPS_DEPS[@]}"; do
    local name="${entry%%|*}"
    [[ -n "${_SHDEPS_CHANGED[$name]+x}" ]] || continue
    local hook_file="$hooks_dir/$name.sh"
    [[ -f "$hook_file" ]] || continue
    unset -f exists version install post uninstall 2>/dev/null
    # shellcheck source=/dev/null
    . "$hook_file" || {
      _shdeps_warn "  warning: failed to source $hook_file"
      continue
    }
    if declare -f post &>/dev/null; then
      post "$name" || true
    fi
    unset -f exists version install post uninstall 2>/dev/null
  done
}

# ---------------------------------------------------------------------------
# Self-update — pull latest shdeps if clean and TTL expired
# ---------------------------------------------------------------------------

# Update the shdeps installation itself (git pull with TTL caching).
# Locates the shdeps source directory from BASH_SOURCE, or accepts an
# explicit directory via $1. Skips dirty working trees (active development).
# Uses its own state dir (not the caller's SHDEPS_STATE_DIR) so the TTL
# stamp is shared across standalone and embedded invocations.
_shdeps_self_update() {
  local shdeps_dir="${1:-${BASH_SOURCE[0]%/*}}"

  if [[ ! -d "$shdeps_dir/.git" ]]; then
    _shdeps_warn "shdeps: not a git clone, cannot self-update"
    return 1
  fi

  # Use shdeps's own state dir for the stamp, not the caller's override,
  # so the TTL is shared with standalone `shdeps self-update` calls.
  local _saved_state="${SHDEPS_STATE_DIR:-}"
  SHDEPS_STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/shdeps"

  local stamp
  stamp=$(_shdeps_remote_stamp "shdeps" "self-update")

  if _shdeps_remote_fresh "$stamp"; then
    SHDEPS_STATE_DIR="$_saved_state"
    return 0
  fi

  # Skip if dirty (uncommitted changes = active development)
  if [[ -n "$(git -C "$shdeps_dir" status --porcelain --untracked-files=normal 2>/dev/null)" ]]; then
    SHDEPS_STATE_DIR="$_saved_state"
    return 0
  fi

  if git -C "$shdeps_dir" pull --ff-only --quiet 2>/dev/null; then
    _shdeps_remote_touch "$stamp" || true
    # Re-source if shdeps.sh was updated so in-memory functions are current
    if [[ -f "$shdeps_dir/shdeps.sh" ]]; then
      SHDEPS_STATE_DIR="$_saved_state"
      # shellcheck source=/dev/null
      . "$shdeps_dir/shdeps.sh"
      return 0
    fi
  else
    _shdeps_warn "shdeps: self-update failed (git pull --ff-only failed)"
    _shdeps_remote_touch "$stamp" || true
  fi

  SHDEPS_STATE_DIR="$_saved_state"
}

# ---------------------------------------------------------------------------
# Parallel binary prefetch
# ---------------------------------------------------------------------------

# Pre-fetch GitHub release JSON for all binary deps in parallel.
# Identifies binary deps with stale TTL, fires background curl requests,
# and stores results in _SHDEPS_PREFETCHED[name]=json for consumption
# by _shdeps_install_binary.
_shdeps_prefetch_binary_releases() {
  declare -gA _SHDEPS_PREFETCHED=()
  _SHDEPS_PREFETCH_READY=1
  command -v curl &>/dev/null || return 0

  local -a _pf_names=() _pf_tmpfiles=() _pf_pids=()
  local -a _pf_curl_args=(curl -fsSL --no-netrc)
  if [[ -n "${_SHDEPS_GH_TOKEN:-}" ]]; then
    _pf_curl_args+=(-H "Authorization: token $_SHDEPS_GH_TOKEN")
  else
    _pf_curl_args+=(-H "Authorization:")
  fi

  local entry
  for entry in "${_SHDEPS_DEPS[@]}"; do
    _shdeps_parse "$entry"
    [[ "$_method" == "binary" ]] || continue
    _shdeps_platform_match "$_platforms" || continue
    _shdeps_host_match "$_hosts" || continue

    local bin_path
    bin_path="$(_shdeps_bin_dir)/$_cmd"
    local stamp
    stamp=$(_shdeps_remote_stamp "$_name" binary)

    # Skip if binary exists and TTL is fresh
    [[ -x "$bin_path" ]] && _shdeps_remote_fresh "$stamp" && continue

    local tmp
    tmp=$(mktemp 2>/dev/null) || continue
    _pf_names+=("$_name")
    _pf_tmpfiles+=("$tmp")

    "${_pf_curl_args[@]}" \
      "https://api.github.com/repos/$_source/releases/latest" \
      >"$tmp" 2>/dev/null &
    _pf_pids+=($!)
  done

  # Wait for all background fetches
  local i
  for i in "${!_pf_pids[@]}"; do
    if wait "${_pf_pids[$i]}" 2>/dev/null; then
      _SHDEPS_PREFETCHED["${_pf_names[$i]}"]=$(<"${_pf_tmpfiles[$i]}")
    fi
    rm -f "${_pf_tmpfiles[$i]}"
  done
}

# Install or upgrade all managed dependencies. Orchestrates:
# 1. Load config and detect package manager
# 2. Install each dep (pkg queues, git/binary install, custom exists/install)
# 3. Flush queued pkg installs
# 4. Run post() hooks for changed deps
_shdeps_update() {
  if ! command -v git &>/dev/null; then
    _shdeps_warn "error: git is required for shdeps"
    return 1
  fi

  _shdeps_load
  _shdeps_pkg_detect
  _SHDEPS_PKG_BATCH=()
  _SHDEPS_PKG_BATCH_NAMES=()
  declare -gA _SHDEPS_CHANGED=()

  _shdeps_log_header "==> Installing/upgrading tools..."

  # Prime sudo early so the password prompt is visible, not buried inside
  # a _shdeps_run_logged redirect where it silently hangs.
  _shdeps_maybe_prime_sudo

  # Refresh package metadata so _shdeps_pkg_available sees all repos.
  # On a fresh machine the cache is empty and packages appear unavailable.
  _shdeps_pkg_refresh_metadata

  # Cache GitHub auth token once (avoids per-dep subprocess overhead).
  _SHDEPS_GH_TOKEN=$(gh auth token 2>/dev/null) || _SHDEPS_GH_TOKEN="${GITHUB_TOKEN:-}"

  # Pre-fetch GitHub release JSON for all binary deps in parallel.
  # Each background curl writes to a temp file; _shdeps_install_binary
  # reads from the associative array instead of making its own API call.
  _shdeps_prefetch_binary_releases

  # Install phase
  local entry
  for entry in "${_SHDEPS_DEPS[@]}"; do
    _shdeps_install_dep "$entry" || true
  done

  # Flush queued pkg installs
  _shdeps_pkg_install_batch

  # Reinstall mode: mark all deps as changed so all post() hooks run.
  # (Install methods already checked shdeps_reinstall individually during step 2.)
  if [[ "$(_shdeps_reinstall)" -eq 1 ]]; then
    for entry in "${_SHDEPS_DEPS[@]}"; do
      _SHDEPS_CHANGED["${entry%%|*}"]=1
    done
  fi

  # Post-install phase
  _shdeps_run_post_hooks

  # Notify about orphaned deps (in manifest but no longer in config)
  if _shdeps_detect_orphans; then
    local _orphan_count=${#_SHDEPS_ORPHANS[@]}
    local _orphan_list="" _o
    for _o in "${_SHDEPS_ORPHANS[@]}"; do
      local _o_name="${_o%%|*}"
      local _o_rest="${_o#*|}"
      local _o_method="${_o_rest%%|*}"
      _orphan_list+="${_orphan_list:+, }$_o_name ($_o_method)"
    done
    _shdeps_warn ""
    _shdeps_warn "==> $_orphan_count orphaned dep(s) (removed from config but still installed):"
    _shdeps_warn "  $_orphan_list"
    _shdeps_warn "  Run: shdeps prune"
  fi
}
