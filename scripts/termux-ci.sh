#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. This job executes the
# NDK-built Android binary inside Termux and verifies Android package policy.
binary=.termux-ci/shdeps

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/conf" "$fixture/state"
printf '%s\n' \
  'termux-runtime pkg android:bash,apt:shdeps-ci-missing android:bash,apt:shdeps-ci-missing os:android' \
  >"$fixture/conf/runtime.conf"

output=$(
  SHDEPS_CONF_DIR="$fixture/conf" \
    SHDEPS_STATE_DIR="$fixture/state" \
    "$binary" list
)
printf '%s\n' "$output"
printf '%s\n' "$output" |
  grep -Eq '^termux-runtime[[:space:]]+pkg[[:space:]]+installed'
