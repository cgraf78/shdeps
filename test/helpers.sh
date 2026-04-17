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

_assert_perms() {
  local desc="$1" expected="$2" path="$3"
  local actual
  if [[ "$(uname)" == "Darwin" ]]; then
    actual=$(stat -f '%Lp' "$path")
  else
    actual=$(stat -c '%a' "$path")
  fi
  if [[ "$actual" == "$expected" ]]; then
    _pass "$desc"
  else
    _fail "$desc (expected perms $expected, got $actual on $path)"
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
# Mocks for external installers (cargo, go)
# ---------------------------------------------------------------------------

# Install a mock `cargo` on PATH that intercepts `cargo install --root <dir>
# <crate> [--force]` and `cargo uninstall --root <dir> <crate>`. Creates
# `<dir>/bin/<crate>` on install and removes it on uninstall. Records each
# invocation to $MOCK_CARGO_LOG (if set). Returns the dir to prepend to PATH.
_mock_cargo_setup() {
  local dir
  dir=$(_tmpdir)
  cat > "$dir/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
subcmd="$1"; shift
root=""; force=0; crate=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)  root="$2"; shift 2 ;;
    --force) force=1; shift ;;
    *)       crate="$1"; shift ;;
  esac
done
if [[ -n "${MOCK_CARGO_LOG:-}" ]]; then
  echo "subcmd=$subcmd root=$root force=$force crate=$crate" >> "$MOCK_CARGO_LOG"
fi
case "$subcmd" in
  install)
    [[ -n "$root" && -n "$crate" ]] || { echo "mock cargo: missing args" >&2; exit 2; }
    mkdir -p "$root/bin"
    cat > "$root/bin/$crate" <<EOF
#!/bin/sh
echo "mock-$crate 1.0.0"
EOF
    chmod +x "$root/bin/$crate"
    printf '[v1]\n"%s 1.0.0" = []\n' "$crate" > "$root/.crates.toml"
    ;;
  uninstall)
    rm -f "$root/bin/$crate" "$root/.crates.toml"
    ;;
  *)
    echo "mock cargo: unsupported subcmd $subcmd" >&2
    exit 2
    ;;
esac
SH
  chmod +x "$dir/cargo"
  echo "$dir"
}

# Install a mock `go` on PATH that handles `go install <module>@<ver>` by
# creating $GOBIN/<basename(module)>, and a minimal `go env <VAR>...` shim.
# Returns the dir to prepend to PATH.
_mock_go_setup() {
  local dir
  dir=$(_tmpdir)
  cat > "$dir/go" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
subcmd="$1"; shift
case "$subcmd" in
  install)
    spec="$1"
    module="${spec%@*}"
    ver="${spec#*@}"
    base="${module##*/}"
    : "${GOBIN:?GOBIN must be set}"
    mkdir -p "$GOBIN"
    cat > "$GOBIN/$base" <<EOF
#!/bin/sh
echo "mock-$base $ver"
EOF
    chmod +x "$GOBIN/$base"
    if [[ -n "${MOCK_GO_LOG:-}" ]]; then
      echo "install module=$module ver=$ver GOBIN=$GOBIN" >> "$MOCK_GO_LOG"
    fi
    ;;
  env)
    for k in "$@"; do
      case "$k" in
        GOBIN)  echo "${GOBIN:-}" ;;
        GOPATH) echo "${GOPATH:-$HOME/go}" ;;
        *)      echo "" ;;
      esac
    done
    ;;
  *)
    echo "mock go: unsupported subcmd $subcmd" >&2
    exit 2
    ;;
esac
SH
  chmod +x "$dir/go"
  echo "$dir"
}

# Install a mock `uv` on PATH that handles `uv tool install [--force] <pkg>`
# (with $UV_TOOL_DIR and $UV_TOOL_BIN_DIR set) by creating
# `$UV_TOOL_BIN_DIR/<pkg>`. Records invocations to $MOCK_UV_LOG (if set).
# Returns the dir to prepend to PATH.
_mock_uv_setup() {
  local dir
  dir=$(_tmpdir)
  cat > "$dir/uv" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == "tool" ]] || { echo "mock uv: expected 'tool' subcommand, got '${1:-}'" >&2; exit 2; }
shift
[[ "${1:-}" == "install" ]] || { echo "mock uv: only supports 'install', got '${1:-}'" >&2; exit 2; }
shift
force=0; pkg=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --force) force=1; shift ;;
    *)       pkg="$1"; shift ;;
  esac
done
: "${UV_TOOL_DIR:?UV_TOOL_DIR must be set}"
: "${UV_TOOL_BIN_DIR:?UV_TOOL_BIN_DIR must be set}"
mkdir -p "$UV_TOOL_DIR" "$UV_TOOL_BIN_DIR"
cat > "$UV_TOOL_BIN_DIR/$pkg" <<EOF
#!/bin/sh
echo "mock-$pkg 1.0.0"
EOF
chmod +x "$UV_TOOL_BIN_DIR/$pkg"
if [[ -n "${MOCK_UV_LOG:-}" ]]; then
  echo "install force=$force pkg=$pkg UV_TOOL_DIR=$UV_TOOL_DIR" >> "$MOCK_UV_LOG"
fi
SH
  chmod +x "$dir/uv"
  echo "$dir"
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
