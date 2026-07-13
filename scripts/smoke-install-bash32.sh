#!/usr/bin/env bash
# Smoke-test the curl-pipe installer path with a local release fixture.
#
# This script is intentionally Bash 3.2-compatible because CI runs it with
# macOS's stock /bin/bash. Keep it small and boring: its job is to prove the
# installer can activate a Rust-era release archive before any modern Bash
# library code is available.

set -euo pipefail

_die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

_script_dir() {
  local src dir
  src="${BASH_SOURCE[0]}"
  case "$src" in
    */*) dir="${src%/*}" ;;
    *) dir="." ;;
  esac
  cd -P -- "$dir" && pwd
}

_sha256_line() {
  local file="$1" name="$2"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | sed "s| .*$|  $name|"
  else
    shasum -a 256 "$file" | sed "s| .*$|  $name|"
  fi
}

_platform_label() {
  local os arch android=0
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m | tr '[:upper:]' '[:lower:]')
  if [[ -n "${ANDROID_ROOT:-}" || -n "${TERMUX_VERSION:-}" || "$(uname -o 2>/dev/null)" == "Android" ]]; then
    android=1
  fi
  case "$arch" in
    amd64) arch="x86_64" ;;
    arm64) arch="aarch64" ;;
  esac

  case "$android:$os:$arch" in
    1:linux:aarch64) printf '%s\n' "android-aarch64" ;;
    0:linux:x86_64) printf '%s\n' "linux-x86_64-musl" ;;
    0:linux:aarch64) printf '%s\n' "linux-aarch64-musl" ;;
    0:darwin:x86_64) printf '%s\n' "macos-x86_64" ;;
    0:darwin:aarch64) printf '%s\n' "macos-aarch64" ;;
    *) _die "unsupported smoke-test platform: $os/$arch" ;;
  esac
}

_tmp_root=$(mktemp -d "${TMPDIR:-/tmp}/shdeps-bash32.XXXXXX")
trap 'rm -rf "$_tmp_root"' EXIT

_repo_dir=$(_script_dir)
_repo_dir="${_repo_dir%/scripts}"
_tag="20990203-040506-abc12345"
_platform=$(_platform_label)
_archive="shdeps-${_tag}-${_platform}.tar.gz"
_release_dir="$_tmp_root/release"
_payload="$_tmp_root/payload"
_fakebin="$_tmp_root/bin"
_installer_dir="$_tmp_root/installer"
_install_dir="$_tmp_root/install/shdeps"
_bin_dir="$_tmp_root/out-bin"
mkdir -p "$_release_dir" "$_payload" "$_fakebin" "$_installer_dir" "$_bin_dir"

cat >"$_payload/shdeps" <<'SH'
#!/usr/bin/env bash
if [[ "${1:-}" == "version" ]]; then
  printf '%s\n' "shdeps 20990203-040506-abc12345"
fi
SH
chmod +x "$_payload/shdeps"

cat >"$_payload/shdeps.sh" <<'SH'
# This release fixture would fail if install.sh tried to source it with Bash
# 3.2. The Rust-era installer must be able to activate the binary first and
# treat sourceable-wrapper extras as optional on old system Bash.
if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 3))); then
  return 42
fi
shdeps_version() { printf '%s\n' "shdeps 20990203-040506-abc12345"; }
SH

cp "$_repo_dir/install.sh" "$_payload/install.sh"
cp "$_repo_dir/install.sh" "$_installer_dir/install.sh"
cp "$_repo_dir/README.md" "$_payload/README.md"
cp "$_repo_dir/LICENSE" "$_payload/LICENSE"
cat >"$_payload/.shdeps-install.json" <<JSON
{"schema":1,"method":"release","artifact_platform":"$_platform","version":"$_tag","tag":"$_tag","commit":"abc123456789","repo":"cgraf78/shdeps"}
JSON

(cd "$_payload" && tar -czf "$_release_dir/$_archive" .)
_sha256_line "$_release_dir/$_archive" "$_archive" >"$_release_dir/$_archive.sha256"

cat >"$_release_dir/latest.json" <<JSON
{
  "tag_name": "$_tag",
  "draft": false,
  "prerelease": false,
  "assets": [
    {"name": "$_archive", "browser_download_url": "https://github.com/cgraf78/shdeps/releases/download/$_tag/$_archive"},
    {"name": "$_archive.sha256", "browser_download_url": "https://github.com/cgraf78/shdeps/releases/download/$_tag/$_archive.sha256"}
  ]
}
JSON

cat >"$_fakebin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
out=""
url=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    -H)
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
case "$url" in
  fixture://latest)
    cp "$SHDEPS_TEST_RELEASE_DIR/latest.json" "$out"
    ;;
  *.sha256)
    cp "$SHDEPS_TEST_RELEASE_DIR/$(basename "$url")" "$out"
    ;;
  *.tar.gz)
    cp "$SHDEPS_TEST_RELEASE_DIR/$(basename "$url")" "$out"
    ;;
  *)
    printf 'unexpected url: %s\n' "$url" >&2
    exit 1
    ;;
esac
SH
chmod +x "$_fakebin/curl"

PATH="$_fakebin:$PATH" \
  SHDEPS_TEST_RELEASE_DIR="$_release_dir" \
  SHDEPS_RELEASE_API_URL="fixture://latest" \
  SHDEPS_DIR="$_install_dir" \
  SHDEPS_BIN="$_bin_dir/shdeps" \
  "$_installer_dir/install.sh" >/dev/null

[[ -x "$_install_dir/shdeps" ]] || _die "release binary was not installed"
[[ -f "$_install_dir/.shdeps-install.json" ]] || _die "install metadata was not installed"
[[ -L "$_bin_dir/shdeps" ]] || _die "CLI symlink was not created"

_version=$("$_bin_dir/shdeps" version)
[[ "$_version" == "shdeps 20990203-040506-abc12345" ]] || _die "unexpected version output: $_version"

printf 'install.sh Bash %s release smoke passed\n' "$BASH_VERSION"
