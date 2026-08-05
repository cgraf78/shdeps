# shellcheck shell=bash
#
# shdeps-specific runtime assertions for the shared smoke-release.sh.
#
# The shared script already checks archive naming, the executable bit, and that
# every declared payload entry shipped. This adds the checks that require
# actually running the artifact, so it is skipped for cross-built android-*
# archives that cannot execute on the runner.

# Find a Bash new enough to source the shipped wrapper.
#
# GitHub macOS runners execute workflow shell steps with /bin/bash 3.2, but the
# wrapper intentionally requires Bash 4.3+. Probe the common Homebrew locations
# before giving up so release smoke tests validate the wrapper contract on macOS
# without weakening that runtime floor.
_shdeps_modern_bash() {
  local candidate candidate_path major minor

  for candidate in "${SHDEPS_SMOKE_BASH:-}" bash /opt/homebrew/bin/bash /usr/local/bin/bash; do
    [[ -n "$candidate" ]] || continue
    if [[ -x "$candidate" ]]; then
      candidate_path=$candidate
    else
      candidate_path=$(command -v "$candidate" 2>/dev/null || true)
    fi
    [[ -n "$candidate_path" ]] || continue

    major=$("$candidate_path" -c "printf '%s\n' \"\${BASH_VERSINFO[0]:-0}\"" 2>/dev/null || true)
    minor=$("$candidate_path" -c "printf '%s\n' \"\${BASH_VERSINFO[1]:-0}\"" 2>/dev/null || true)
    [[ "$major" =~ ^[0-9]+$ && "$minor" =~ ^[0-9]+$ ]] || continue
    if ((major > 4 || (major == 4 && minor >= 3))); then
      printf '%s\n' "$candidate_path"
      return 0
    fi
  done

  return 1
}

release_smoke_check() {
  local root=$1
  local wrapper_bash

  "$root/shdeps" version
  "$root/shdeps" help >/dev/null

  wrapper_bash=$(_shdeps_modern_bash) || {
    printf 'release smoke: shdeps.sh requires Bash 4.3+; set SHDEPS_SMOKE_BASH to a compatible bash\n' >&2
    return 1
  }
  # Single quotes are deliberate: the wrapper path is passed as $1 so the inner
  # Bash expands it, not this shell.
  # shellcheck disable=SC2016
  "$wrapper_bash" -c '. "$1"; shdeps_version >/dev/null' bash "$root/shdeps.sh"
}
