#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: scripts/smoke-release.sh <asset-platform>\n' >&2
  exit 2
fi

asset_platform=$1
tag=$(scripts/release-tag.sh)
archive="dist/shdeps-${tag}-${asset_platform}.tar.gz"
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
test -f "$smoke/completions/shdeps.bash"
test -f "$smoke/completions/shdeps.zsh"
test -f "$smoke/completions/shdeps.fish"

"$smoke/shdeps" version
"$smoke/shdeps" help >/dev/null

bash -c '. "$1"; shdeps_version >/dev/null' bash "$smoke/shdeps.sh"
