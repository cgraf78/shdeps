#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# Exercise both the native Rust suite and the runtime path that distinguishes
# Android's APT from a conventional Debian-family host.
pkg update -y
pkg install -y rust
cargo test --locked
cargo build --locked

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/conf" "$fixture/state"
printf '%s\n' \
  'termux-runtime pkg android:bash,apt:shdeps-ci-missing android:bash,apt:shdeps-ci-missing os:android' \
  >"$fixture/conf/runtime.conf"

output=$(
  SHDEPS_CONF_DIR="$fixture/conf" \
    SHDEPS_STATE_DIR="$fixture/state" \
    target/debug/shdeps list
)
printf '%s\n' "$output"
printf '%s\n' "$output" |
  grep -Eq '^termux-runtime[[:space:]]+pkg[[:space:]]+installed'
