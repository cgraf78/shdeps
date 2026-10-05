#!/usr/bin/env bash
# helpers.sh — shared test framework for shdeps tests.
#
# Source this file from shell test scripts to get assertion helpers,
# temp directory management, and a summary reporter.
#
# Usage:
#   . "$(dirname "$0")/helpers.sh"
#   _assert_eq "description" "expected" "actual"
#   ...
#   _test_summary  # prints results, exits 0 or 1

PASS=0
FAIL=0
_SHDEPS_TEST_TMP_ROOT=$(mktemp -d) || exit 1

# install.sh publishes the physical Lua tree so a stable link never depends on
# an intermediate directory symlink. macOS commonly exposes this distinction:
# mktemp spells paths as /var/..., while `cd -P` resolves them to /private/var/....
# Compare physical directory names in tests that inspect the link target.
_physical_dir() {
  (cd -P -- "$1" && pwd)
}

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

_pass() {
  PASS=$((PASS + 1))
  echo "  PASS: $1"
}
_fail() {
  FAIL=$((FAIL + 1))
  echo "  FAIL: $1" >&2
}

_assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected '$expected', got '$actual')"
  fi
}

_assert_contains() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected to contain '$expected', got '$actual')"
  fi
}

_assert_not_contains() {
  local desc="$1" unexpected="$2" actual="$3"
  if [[ "$actual" != *"$unexpected"* ]]; then
    _pass "$desc"
  else
    _fail "$desc (should not contain '$unexpected')"
  fi
}

_assert_exit() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" -eq "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected exit $expected, got $actual)"
  fi
}

_assert_file_exists() {
  local desc="$1" path="$2"
  if [[ -f "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (file not found: $path)"
  fi
}

_assert_file_missing() {
  local desc="$1" path="$2"
  if [[ ! -f "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (file should not exist: $path)"
  fi
}

_assert_dir_exists() {
  local desc="$1" path="$2"
  if [[ -d "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (dir not found: $path)"
  fi
}

_assert_symlink() {
  local desc="$1" path="$2"
  if [[ -L "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (not a symlink: $path)"
  fi
}

# Assert nothing exists at $path — no regular file, no directory, no symlink
# (including broken symlinks, which `_assert_file_missing`'s `! -f` misses).
_assert_not_exists() {
  local desc="$1" path="$2"
  if [[ ! -e "$path" && ! -L "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (path should not exist: $path)"
  fi
}

_assert_dir_missing() {
  local desc="$1" path="$2"
  if [[ ! -d "$path" ]]; then
    _pass "$desc"
  else
    _fail "$desc (dir should not exist: $path)"
  fi
}

_assert_file_content() {
  local desc="$1" expected="$2" path="$3"
  if [[ -f "$path" ]]; then
    local actual
    actual=$(cat "$path")
    if [[ "$actual" == "$expected" ]]; then
      _pass "$desc"
    else
      _fail "$desc (expected content '$expected', got '$actual')"
    fi
  else
    _fail "$desc (file not found: $path)"
  fi
}

_assert_match() {
  local desc="$1" pattern="$2" actual="$3"
  if [[ "$actual" =~ $pattern ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected to match '$pattern', got '$actual')"
  fi
}

# ---------------------------------------------------------------------------
# Temp directory management
# ---------------------------------------------------------------------------

_tmpdir() {
  mktemp -d "$_SHDEPS_TEST_TMP_ROOT/fixture.XXXXXX"
}

_cleanup() {
  rm -rf "$_SHDEPS_TEST_TMP_ROOT"
}
trap _cleanup EXIT

# ---------------------------------------------------------------------------
# Common test setup
# ---------------------------------------------------------------------------

# Create a mock HOME, saving the original. Sets TEST_HOME, REAL_HOME, HOME.
_mock_home() {
  # shellcheck disable=SC2034  # REAL_HOME is used by callers
  REAL_HOME="$HOME"
  TEST_HOME=$(_tmpdir)
  export HOME="$TEST_HOME"
  # Set git identity for test commits (CI has no global config)
  git config --global user.email "test@test.com"
  git config --global user.name "Test"
}

# Create a temp bin directory for mock commands. Returns the path.
_mock_bin() {
  local d
  d=$(_tmpdir)
  echo "$d"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

_test_summary() {
  echo ""
  echo "================================"
  echo "Results: $PASS passed, $FAIL failed"
  echo "================================"
  [[ $FAIL -eq 0 ]] && exit 0 || exit 1
}
