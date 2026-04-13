#!/usr/bin/env bash
# helpers.sh — shared test framework for shdeps tests.
#
# Source this file from test scripts to get assertion helpers,
# temp directory management, and a summary reporter.
#
# Usage:
#   . "$(dirname "$0")/helpers.sh"
#   _assert_eq "description" "expected" "actual"
#   ...
#   _test_summary  # prints results, exits 0 or 1

PASS=0
FAIL=0
CLEANUP_DIRS=()

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

_assert_neq() {
  local desc="$1" unexpected="$2" actual="$3"
  if [[ "$unexpected" != "$actual" ]]; then
    _pass "$desc"
  else
    _fail "$desc (should not equal '$unexpected')"
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
  local d
  d=$(mktemp -d)
  CLEANUP_DIRS+=("$d")
  echo "$d"
}

_cleanup() {
  for d in "${CLEANUP_DIRS[@]+"${CLEANUP_DIRS[@]}"}"; do
    rm -rf "$d"
  done
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

# Source shdeps.sh with test-friendly defaults.
# Call after _mock_home to isolate state.
_source_shdeps() {
  local shdeps_dir="${1:-$SHDEPS_DIR}"
  # Reset any previously defined functions/vars
  unset -f _shdeps_log _shdeps_warn _shdeps_log_ok _shdeps_log_dim _shdeps_log_header 2>/dev/null
  unset _SHDEPS_DEPS _SHDEPS_PKG_MGR _SHDEPS_PKG_BATCH _SHDEPS_PKG_BATCH_NAMES 2>/dev/null
  unset _SHDEPS_CHANGED 2>/dev/null
  # Suppress all output during tests
  export SHDEPS_LOG_LEVEL=0
  # shellcheck source=/dev/null
  . "$shdeps_dir/shdeps.sh"
}

# ---------------------------------------------------------------------------
# Fake release JSON builder (for binary asset matching tests)
# ---------------------------------------------------------------------------

_fake_release_json() {
  local result='{"assets":['
  local first=1
  local name
  for name in "$@"; do
    [[ $first -eq 0 ]] && result+=","
    first=0
    result+='{"browser_download_url":"https://github.com/test/tool/releases/download/v1.0.0/'"$name"'"}'
  done
  result+=']}'
  echo "$result"
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
