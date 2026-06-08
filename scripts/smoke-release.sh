#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: scripts/smoke-release.sh <asset-platform>\n' >&2
  exit 2
fi

asset_platform=$1
case "$asset_platform" in
  *unknown*)
    # Smoke tests run against the same label that gets uploaded. Reject target
    # triples here too so a workflow edit cannot package a clean label but smoke
    # a different, Rust-internal archive name by mistake.
    printf 'asset platform must be a public release label, not a Rust target triple: %s\n' "$asset_platform" >&2
    exit 2
    ;;
esac

modern_bash() {
  local candidate candidate_path major minor

  # GitHub macOS runners execute workflow shell steps with /bin/bash 3.2, but
  # the shipped sourceable wrapper intentionally requires Bash 4.3+. Probe the
  # common Homebrew locations before giving up so release smoke tests validate
  # the wrapper contract on macOS without weakening that runtime floor.
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

tag=$(scripts/release-tag.sh)
archive="dist/shdeps-${tag}-${asset_platform}.tar.gz"
if [[ ! -f "$archive" && -f dist/.shdeps-release-version ]]; then
  # Local dry-runs are not anchored by a pushed release tag. Reuse the package
  # script's recorded version so smoke always verifies the archive produced by
  # the immediately preceding package step.
  tag=$(<dist/.shdeps-release-version)
  case "$tag" in
    '' | *[!A-Za-z0-9._-]*)
      printf 'recorded release version is unsafe for asset names: %s\n' "$tag" >&2
      exit 2
      ;;
  esac
  archive="dist/shdeps-${tag}-${asset_platform}.tar.gz"
fi
smoke=$(mktemp -d)

cleanup() {
  rm -rf "$smoke"
}
trap cleanup EXIT

tar -xzf "$archive" -C "$smoke"

# Keep the smoke test intentionally small and user-facing. Unit tests already
# cover extraction safety and metadata parsing; this catches packaging mistakes
# such as a missing executable bit, omitted wrapper, or archive name drift.
test -x "$smoke/shdeps"
test -f "$smoke/shdeps.sh"
test -x "$smoke/install.sh"
test -f "$smoke/.shdeps-install.json"
test -f "$smoke/man/man1/shdeps.1"
test -f "$smoke/lua/shdeps.lua"
test -f "$smoke/lua/shdeps/core.lua"
test -f "$smoke/lua/shdeps/bootstrap.lua"
test -f "$smoke/completions/shdeps.bash"
test -f "$smoke/completions/shdeps.zsh"
test -f "$smoke/completions/shdeps.fish"

"$smoke/shdeps" version
"$smoke/shdeps" help >/dev/null

bash_for_wrapper=$(modern_bash) || {
  printf 'release smoke: shdeps.sh requires Bash 4.3+; set SHDEPS_SMOKE_BASH to a compatible bash\n' >&2
  exit 1
}
"$bash_for_wrapper" -c ". \"\$1\"; shdeps_version >/dev/null" bash "$smoke/shdeps.sh"
