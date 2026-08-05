#!/usr/bin/env bash
# Install, update, or bootstrap shdeps.
#
# Usage:
#   curl -fsSL .../install.sh | bash          # install/update
#   . /path/to/install.sh --bootstrap         # source into caller
#   ./install.sh --uninstall                  # remove
#
# Environment:
#   SHDEPS_DIR          Install directory      (default: ~/.local/share/shdeps)
#   SHDEPS_REPO         Git repo URL for source/dev mode
#                       (default: https://github.com/cgraf78/shdeps.git)
#   SHDEPS_BIN          CLI symlink path       (default: ~/.local/bin/shdeps)
#   SHDEPS_LIB          Direct path to shdeps.sh for explicit/dev bootstrap use
#   SHDEPS_GIT_DEV_DIR  Dev clone directory    (default: ~/git)

# Strict mode when executed directly; skip when sourced (--bootstrap)
# to avoid infecting the caller's shell options.
_SHDEPS_INSTALL_EXECUTED=0
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  _SHDEPS_INSTALL_EXECUTED=1
  set -euo pipefail
fi

_SHDEPS_DEFAULT_REPO="https://github.com/cgraf78/shdeps.git"
SHDEPS_DIR="${SHDEPS_DIR:-$HOME/.local/share/shdeps}"
SHDEPS_REPO="${SHDEPS_REPO:-$_SHDEPS_DEFAULT_REPO}"
SHDEPS_BIN="${SHDEPS_BIN:-$HOME/.local/bin/shdeps}"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_info() { printf '%s\n' "$*" >&2; }
_error() { printf 'error: %s\n' "$*" >&2; }

_install_status_requires_propagation() {
  # Status 125 means transaction cleanup could not prove that recovery state
  # is safe. It is as non-recoverable as cancellation at wrapper boundaries:
  # best-effort refresh must never downgrade or silently ignore it.
  case "${1:-0}" in
    125 | 129 | 130 | 143) return 0 ;;
    *) return 1 ;;
  esac
}

# Record the first transaction signal and cooperatively stop its exact active
# child. The normal control path owns bounded escalation, reaping, and cleanup.
_install_transaction_signal() {
  local signal="$1" status

  case "$signal" in
    HUP) status=129 ;;
    INT) status=130 ;;
    TERM) status=143 ;;
    *) return 0 ;;
  esac
  if [[ "${_shdeps_tx_signal:-0}" -eq 0 ]]; then
    _shdeps_tx_signal="$status"
    if [[ -n "${_shdeps_tx_child:-}" ]]; then
      kill -TERM "$_shdeps_tx_child" 2>/dev/null || true
    fi
  fi
}

_install_transaction_install_signal_traps() {
  trap '_install_transaction_signal HUP' HUP
  trap '_install_transaction_signal INT' INT
  trap '_install_transaction_signal TERM' TERM
}

_install_transaction_job_running() {
  local wanted="$1" pid

  while IFS= read -r pid; do
    [[ "$pid" == "$wanted" ]] && return 0
  done < <(jobs -pr)
  return 1
}

# TERM/KILL and reap the still-owned child after a signal interrupted wait.
_install_transaction_cancel_child() {
  local remaining=50

  [[ -n "${_shdeps_tx_child:-}" ]] || return 0
  if _install_transaction_job_running "$_shdeps_tx_child"; then
    kill -TERM "$_shdeps_tx_child" 2>/dev/null || true
    while ((remaining > 0)); do
      _install_transaction_job_running "$_shdeps_tx_child" || break
      sleep 0.02
      remaining=$((remaining - 1))
    done
    if _install_transaction_job_running "$_shdeps_tx_child"; then
      kill -KILL "$_shdeps_tx_child" 2>/dev/null || true
    fi
  fi
  wait "$_shdeps_tx_child" 2>/dev/null || true
  _shdeps_tx_child=""
}

# Preserve a normal child failure, but give a latched signal precedence after
# cancelling and reaping any child whose wait was interrupted.
_install_transaction_wait_child() {
  local rc=0

  while :; do
    if wait "$_shdeps_tx_child"; then
      rc=0
      _shdeps_tx_child=""
      break
    else
      rc=$?
      if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
        _install_transaction_cancel_child
        return "$_shdeps_tx_signal"
      fi
      # A caller-owned trap such as WINCH or USR1 can interrupt Bash's wait
      # without stopping the exact child. Retain ownership and wait again;
      # clearing the slot here would let cleanup race a live curl/cp/tar.
      if _install_transaction_job_running "$_shdeps_tx_child"; then
        continue
      fi
      # The child may have completed while a longer caller trap ran. Collect
      # its retained wait status instead of returning the unrelated signal's
      # synthetic 128+N status as the transaction result.
      if wait "$_shdeps_tx_child"; then
        _shdeps_tx_child=""
        break
      else
        rc=$?
      fi
      _shdeps_tx_child=""
      return "$rc"
    fi
  done
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    return "$_shdeps_tx_signal"
  fi
}

_install_transaction_wait_started_child() {
  _shdeps_tx_child="$1"
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    _install_transaction_cancel_child
    return "$_shdeps_tx_signal"
  fi
  _install_transaction_wait_child
}

# Run one exact external child. A trapped signal interrupts wait immediately;
# the unreaped PID remains owned until bounded cancellation completes.
_install_transaction_run() {
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    return "$_shdeps_tx_signal"
  fi
  "$@" &
  _install_transaction_wait_started_child "$!"
}

_install_transaction_run_to_file() {
  local output="$1"
  shift
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    return "$_shdeps_tx_signal"
  fi
  "$@" >"$output" &
  _install_transaction_wait_started_child "$!"
}

_install_transaction_run_with_input() {
  local input="$1"
  shift
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    return "$_shdeps_tx_signal"
  fi
  "$@" <<<"$input" &
  _install_transaction_wait_started_child "$!"
}

_install_run() {
  if [[ "${_shdeps_tx_active:-0}" -eq 1 ]]; then
    _install_transaction_run "$@"
  else
    "$@"
  fi
}

_install_run_to_file() {
  local output="$1"
  shift
  if [[ "${_shdeps_tx_active:-0}" -eq 1 ]]; then
    _install_transaction_run_to_file "$output" "$@"
  else
    "$@" >"$output"
  fi
}

_install_run_with_input() {
  local input="$1"
  shift
  if [[ "${_shdeps_tx_active:-0}" -eq 1 ]]; then
    _install_transaction_run_with_input "$input" "$@"
  else
    "$@" <<<"$input"
  fi
}

_bash_supports_sourceable_wrapper() {
  ((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3)))
}

_check_source_prereqs() {
  if ! command -v git >/dev/null 2>&1; then
    _error "git is required"
    return 1
  fi

  # Source-checkout installs are developer/explicit mode. Normal fleet
  # bootstrap should use release assets and therefore should not need Git,
  # Cargo, or a source tree at all.
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

_is_bundle_dir() {
  local dir="$1"
  [[ -x "$dir/shdeps" && -f "$dir/.shdeps-install.json" ]]
}

_is_release_install_dir() {
  local dir="$1"

  [[ -d "$dir" && ! -d "$dir/.git" ]] || return 1
  grep -q '"method"[[:space:]]*:[[:space:]]*"release"' "$dir/.shdeps-install.json" 2>/dev/null
}

_release_binary_supports_wrapper_abi() {
  local dir="$1" version=""

  [[ -x "$dir/shdeps" ]] || return 1
  version=$("$dir/shdeps" __api version 2>/dev/null) || return 1
  [[ "$version" == "abi:1" ]]
}

_repair_release_if_needed() {
  local dir="$1" rc=0

  _is_release_install_dir "$dir" || return 0
  if ! _release_binary_supports_wrapper_abi "$dir"; then
    # Unlike a normal forced freshness check, this archive cannot service the
    # wrapper at all. Do not fall through and emit a misleading ABI error after
    # a failed repair download.
    _install_release >/dev/null 2>&1 || rc=$?
    if _install_status_requires_propagation "$rc"; then return "$rc"; fi
    [[ "$rc" -eq 0 ]] || return 1
  elif [[ "${SHDEPS_FORCE:-0}" == 1 ]]; then
    # Preserve the existing best-effort force behavior: a transient GitHub
    # failure must not disable an otherwise compatible local release.
    _install_release >/dev/null 2>&1 || rc=$?
    if _install_status_requires_propagation "$rc"; then return "$rc"; fi
  fi
}

_is_source_checkout_dir() {
  local dir="$1"

  # A curl-pipe installer is executed from the caller's current directory. In
  # CI that directory is often another Git checkout, such as dotfiles. Treating
  # any `.git` directory as "this script is running from a shdeps source
  # checkout" accidentally routes fresh installs into source mode and
  # makes release-capable machines need Cargo. A real shdeps checkout has both
  # the installer and sourceable library beside the Git metadata, so use that as
  # the ownership signal instead of the caller's unrelated repository shape.
  [[ -d "$dir/.git" && -f "$dir/install.sh" && -f "$dir/shdeps.sh" ]]
}

_bootstrap_lib_is_installed_tree() {
  local lib_dir install_dir

  [[ -n "${SHDEPS_LIB:-}" && -f "$SHDEPS_LIB" ]] || return 1
  [[ "${SHDEPS_LIB##*/}" == "shdeps.sh" ]] || return 1

  # Some callers historically exported SHDEPS_LIB after finding the default
  # installed tree. That is not a true override: it points at the same tree
  # bootstrap would have inspected anyway. Normalize both directories so those
  # callers still get release migration instead of pinning the old source
  # checkout layout forever.
  lib_dir="${SHDEPS_LIB%/*}"
  lib_dir=$(cd -P -- "$lib_dir" 2>/dev/null && pwd) || return 1
  install_dir=$(cd -P -- "$SHDEPS_DIR" 2>/dev/null && pwd) || return 1
  [[ "$lib_dir" == "$install_dir" ]]
}

_github_token() {
  local destination="$1" selected=""

  if [[ -n "${GH_TOKEN:-}" ]]; then
    selected="$GH_TOKEN"
  elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
    selected="$GITHUB_TOKEN"
  fi
  # The bootstrap transaction deliberately does not probe `gh auth token`.
  # Credential helpers can block outside its exact-child ownership, while the
  # public release path needs no token. Private/custom API callers can provide
  # GH_TOKEN or GITHUB_TOKEN explicitly; the steady-state Rust client retains
  # its own bounded credential lookup.
  printf -v "$destination" '%s' "$selected"
}

_curl_get() {
  local url="$1" out="$2" token="${3:-}" config
  case "$url" in
    https://api.github.com/*)
      if [[ -n "$token" ]]; then
        # Feed the request via `curl --config -` over stdin so the bearer
        # token never appears in argv, where `ps` / `/proc/<pid>/cmdline`
        # would expose it to any other user on the host. `printf` is a
        # Bash builtin, so the token stays inside this process and never
        # reaches a child argv. Mirrors `_curl_get_release_asset` and the
        # Rust `src/http.rs::curl_config` path; each value is wrapped in
        # `"..."` with `\` and `"` escaped per curl's config syntax.
        local _url_escaped _token_escaped
        _url_escaped=$(_curl_config_quote "$url") || return 1
        _token_escaped=$(_curl_config_quote "$token") || {
          _error "refusing GitHub token containing a newline"
          return 1
        }
        printf -v config '%s\n%s\n%s\n%s\n' \
          "url = \"$_url_escaped\"" \
          'user-agent = "shdeps"' \
          'header = "Accept: application/vnd.github+json"' \
          "header = \"Authorization: Bearer $_token_escaped\""
        _install_run_with_input "$config" curl -fsSL --config - -o "$out"
        return
      fi
      # No token: a plain header on argv is harmless (nothing secret).
      _install_run curl -fsSL -A shdeps -H "Accept: application/vnd.github+json" -o "$out" "$url"
      ;;
    *)
      _install_run curl -fsSL -A shdeps -o "$out" "$url"
      ;;
  esac
}

_curl_get_release_asset() {
  local browser_url="$1" out="$2" token="${3:-}" api_url="${4:-}"
  local config have_api_fallback=0 _bearer_token_escaped _api_url_escaped
  if [[ -n "$token" && -n "$api_url" ]]; then
    have_api_fallback=1
  fi

  # Validate the browser URL host before any fetch, mirroring
  # `is_safe_release_asset_url` in `src/github.rs`. For the SHDEPS_RELEASE_API_URL
  # path the browser URL is parsed out of release JSON, so a tampered or
  # malformed payload could otherwise redirect the archive/checksum download to
  # an arbitrary host with only the checksum as a backstop. curl transparently
  # follows GitHub's 30x into `objects.githubusercontent.com`, so callers only
  # ever supply the canonical `github.com` form; accept both known-good hosts.
  case "$browser_url" in
    https://github.com/* | https://objects.githubusercontent.com/*) ;;
    *)
      _error "refusing to download release asset from non-GitHub host: $browser_url"
      return 1
      ;;
  esac

  # Public `browser_download_url` downloads must look like ordinary
  # browser downloads; GitHub's signed storage redirects reject
  # forwarded API headers. Try the browser URL first regardless of
  # whether a token is available. Suppress stderr only when we have a
  # private-release API fallback available — otherwise the user
  # should see the underlying curl error message so a permanent
  # network problem is not swallowed.
  if [[ "$have_api_fallback" -eq 1 ]]; then
    if _curl_get "$browser_url" "$out" "" 2>/dev/null; then
      return 0
    fi
  else
    if _curl_get "$browser_url" "$out" ""; then
      return 0
    fi
    # No API fallback to try — propagate the browser failure.
    return 1
  fi

  # Browser URL failed; try GitHub's REST asset endpoint with the
  # private-asset headers, matching GitHub's documented private
  # release download flow.
  #
  # Validate the api_url is actually a GitHub API host before
  # attaching the bearer token. The Rust `download_asset` enforces
  # the same prefix check via `is_safe_api_asset_url`; without
  # mirroring it here, a caller (or a tampered release JSON) that
  # supplied a non-GitHub `api_url` would leak the bearer token to
  # that host. Bash bootstrap inputs are normally constructed from
  # the release JSON shdeps just verified, but defense-in-depth
  # keeps the two implementations consistent.
  case "$api_url" in
    https://api.github.com/*) ;;
    *)
      _error "refusing authenticated download from non-GitHub API host: $api_url"
      return 1
      ;;
  esac
  # Feed the request via `curl --config -` over stdin so the bearer
  # token never appears in argv (where it would be visible to `ps`
  # / `/proc/<pid>/cmdline` for any other user on the same host).
  # The here-string keeps the token out of child argv. Older Bash may back that
  # stdin with a private, unlinked temporary object, but no named token file is
  # left for transaction cleanup.
  # Mirrors the same pattern `src/http.rs::curl_config` uses for the
  # steady-state Rust path. Each value is wrapped in `"..."` with
  # `\` and `"` backslash-escaped per curl's config syntax.
  _bearer_token_escaped=$(_curl_config_quote "$token") || {
    _error "refusing GitHub token containing a newline"
    return 1
  }
  _api_url_escaped=$(_curl_config_quote "$api_url") || return 1
  printf -v config '%s\n%s\n%s\n%s\n' \
    "url = \"$_api_url_escaped\"" \
    'user-agent = "shdeps"' \
    'header = "Accept: application/octet-stream"' \
    "header = \"Authorization: Bearer $_bearer_token_escaped\""
  _install_run_with_input "$config" curl -fsSL --config - -o "$out"
}

# Escape a value for inclusion inside a `"..."`-quoted curl
# `--config` field. curl's config format treats `\` as an escape
# character and `"` as the field terminator, so both must be
# doubled. Newlines are rejected because they would create a second config
# directive instead of remaining part of the quoted value.
_curl_config_quote() {
  local value="$1"
  case "$value" in
    *$'\n'* | *$'\r'*) return 1 ;;
  esac
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "$value"
}

_repo_slug() {
  _repo_url_slug "$SHDEPS_REPO"
}

_repo_url_slug() {
  local repo="$1"

  # The same GitHub repository can be spelled several ways in bootstrap
  # contexts: public HTTPS, normal SSH, or a GitHub SSH host alias from a
  # caller's custom config. Compare
  # ownership by owner/repo slug so install policy does not drift based on the
  # transport required to authenticate, but keep the normalization GitHub-scoped
  # so an unrelated host with the same path is not silently treated as ours.
  case "$repo" in
    https://github.com/*) repo="${repo#https://github.com/}" ;;
    ssh://git@github.com/*) repo="${repo#ssh://git@github.com/}" ;;
    git@github.com:*) repo="${repo#git@github.com:}" ;;
    git@github.com-*:*) repo="${repo#*:}" ;;
  esac
  repo="${repo%.git}"
  printf '%s\n' "$repo"
}

_uses_default_repo_slug() {
  [[ "$(_repo_slug)" == "$(_repo_url_slug "$_SHDEPS_DEFAULT_REPO")" ]]
}

_is_android() {
  [[ -n "${ANDROID_ROOT:-}" || -n "${TERMUX_VERSION:-}" ]] && return 0
  [[ "$(uname -o 2>/dev/null)" == "Android" ]]
}

_release_platform() {
  local os arch android=0
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m | tr '[:upper:]' '[:lower:]')
  _is_android && android=1
  case "$arch" in
    amd64) arch="x86_64" ;;
    arm64) arch="aarch64" ;;
  esac

  case "$android:$os:$arch" in
    1:linux:aarch64) printf '%s\n' "android-aarch64" ;;
    1:linux:x86_64) printf '%s\n' "android-x86_64" ;;
    1:linux:*)
      _error "unsupported shdeps Android release architecture: $arch"
      return 1
      ;;
    0:linux:x86_64) printf '%s\n' "linux-x86_64-musl" ;;
    0:linux:aarch64) printf '%s\n' "linux-aarch64-musl" ;;
    0:darwin:x86_64) printf '%s\n' "macos-x86_64" ;;
    0:darwin:aarch64) printf '%s\n' "macos-aarch64" ;;
    *)
      _error "unsupported shdeps release platform: $os/$arch"
      return 1
      ;;
  esac
}

_json_string() {
  local file="$1" key="$2"
  sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" "$file" | head -n 1
}

_asset_url() {
  local file="$1" name="$2"

  # GitHub release asset objects contain nested objects such as `uploader`
  # before `browser_download_url`. A simple "reset on any closing brace" parser
  # mistakes that nested brace for the end of the asset and silently misses real
  # assets. Once the exact asset name is seen, keep scanning forward to its
  # download URL; GitHub always emits that URL in the same asset object.
  awk -v wanted="$name" '
    $0 ~ "\"name\"[[:space:]]*:[[:space:]]*\"" wanted "\"" { in_asset = 1 }
    in_asset && /"browser_download_url"[[:space:]]*:/ {
      sub(/^.*"browser_download_url"[[:space:]]*:[[:space:]]*"/, "")
      sub(/".*$/, "")
      print
      exit
    }
  ' "$file"
}

_asset_api_url() {
  local file="$1" name="$2"

  awk -v wanted="$name" '
    /"url"[[:space:]]*:/ {
      value = $0
      sub(/^.*"url"[[:space:]]*:[[:space:]]*"/, "", value)
      sub(/".*$/, "", value)
      if (value ~ /^https:\/\/api\.github\.com\/repos\/.*\/releases\/assets\/[0-9]+$/) {
        asset_api_url = value
      }
    }
    $0 ~ "\"name\"[[:space:]]*:[[:space:]]*\"" wanted "\"" && asset_api_url != "" {
      print asset_api_url
      exit
    }
  ' "$file"
}

_latest_release_tag() {
  local repo="$1" effective tag output

  # The default shdeps repo is public, so bootstrap should not burn scarce
  # unauthenticated GitHub API quota just to learn the current tag. The normal
  # release page redirects to `/releases/tag/<tag>` and works for curl-pipe
  # installs without requiring `gh`, JSON parsing, or a token.
  if [[ "${_shdeps_tx_active:-0}" -eq 1 ]]; then
    output="$_shdeps_tx_release_path/latest-url"
    if ! _install_run_to_file "$output" curl -fsSL -A shdeps -o /dev/null \
      -w '%{url_effective}' "https://github.com/$repo/releases/latest"; then
      return 1
    fi
    # Keep the small response inside transaction-owned release scratch. A
    # builtin read avoids an untracked `cat` and another child after a signal;
    # the exact scratch directory is removed by normal transaction cleanup.
    IFS= read -r effective <"$output" || true
    if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
      return "$_shdeps_tx_signal"
    fi
  else
    effective=$(curl -fsSL -A shdeps -o /dev/null -w '%{url_effective}' \
      "https://github.com/$repo/releases/latest") || return 1
  fi
  case "$effective" in
    */releases/tag/*) ;;
    *) return 1 ;;
  esac
  tag="${effective##*/releases/tag/}"
  tag="${tag%%[?#]*}"
  [[ -n "$tag" ]] || return 1
  _SHDEPS_LATEST_RELEASE_TAG="$tag"
}

_verify_checksum() {
  local dir="$1" archive="$2" checksum="$3"
  local hasher="" actual="" expected=""
  local output="$dir/.shdeps-checksum.actual" rc=0

  if command -v sha256sum >/dev/null 2>&1; then
    hasher="sha256sum"
  elif command -v shasum >/dev/null 2>&1; then
    hasher="shasum"
  else
    _error "sha256sum or shasum is required to verify release archives"
    return 1
  fi

  # Bind the digest to the exact archive we are about to extract. `<hasher> -c`
  # alone would pass if ANY file named in the checksum file verifies, never
  # binding the result to `$archive` — a malformed or release-wide checksum
  # file could satisfy `-c` without the archive bytes ever being checked.
  # Instead, compute the digest of `$archive` directly and compare it to the
  # checksum line that names `$archive`, mirroring `src/checksum.rs::verify`.
  if [[ "$hasher" == sha256sum ]]; then
    _install_run_to_file "$output" sha256sum "$dir/$archive" 2>/dev/null || rc=$?
  else
    _install_run_to_file "$output" shasum -a 256 "$dir/$archive" 2>/dev/null || rc=$?
  fi
  if [[ "$rc" -ne 0 ]]; then
    # A latched signal owns cleanup of the release scratch as a whole. Do not
    # start another normal child after cancellation merely to remove this file.
    if [[ "${_shdeps_tx_signal:-0}" -eq 0 ]]; then
      rm -f -- "$output"
    fi
    return "$rc"
  fi
  IFS= read -r actual <"$output" || actual=""
  actual="${actual%%[[:space:]]*}"
  _install_run rm -f -- "$output" || return $?
  if [[ -z "$actual" ]]; then
    _error "failed to compute checksum for $archive"
    return 1
  fi

  # Extract the expected digest from the line whose filename field matches
  # `$archive`. Accept the standard `<hash>  <file>` and binary-mode
  # `<hash> *<file>` forms plus a leading `./`, matching
  # `checksum::named_checksum_file`. Bare-hash lines (no filename) are ignored,
  # preserving the per-file binding. The bootstrap only ever consumes a
  # per-archive `<archive>.sha256` (sha256sum output, digest first); the Rust
  # path additionally tolerates filename-first multi-digest manifests, which
  # this code path never receives.
  expected=$(awk -v want="$archive" '
    NF >= 2 {
      name = $2
      sub(/^[*]/, "", name)
      sub(/^\.\//, "", name)
      if (name == want) { print tolower($1); exit }
    }
  ' "$dir/$checksum" 2>/dev/null)
  if [[ -z "$expected" ]]; then
    _error "checksum file has no entry for $archive"
    return 1
  fi

  if [[ "$(printf '%s' "$actual" | tr 'A-F' 'a-f')" != "$expected" ]]; then
    return 1
  fi
}

# Returns 0 (safe) only when every entry in a tar.gz archive uses a
# relative path with no `..` components AND is a regular file or
# directory (no symlinks or hardlinks). The name checks use `tar -tzf`
# and the type check uses `tar -tzvf`; both listings are supported by
# GNU tar and the BSD tar shipped on macOS, so the bootstrap does not
# depend on a platform-specific `--no-absolute-filenames` flag.
#
# This mirrors the Rust extractor's structural defense
# (`archive::reject_links`): a symlink/hardlink entry can redirect a
# later extracted file outside the bundle even when its own name looks
# benign. The curl-pipe path also verifies the whole-archive SHA-256
# before extraction, but the link rejection here means parity no longer
# depends on the publisher's `.sha256` being uncompromised.
_archive_entries_safe() {
  local archive="$1" entry names verbose
  if ! command -v tar >/dev/null 2>&1; then
    return 1
  fi
  names="${archive}.entries"
  verbose="${archive}.verbose"
  # Pass 1: reject absolute paths and `..` traversal. Run on the clean
  # name-only listing so the well-tested traversal globs operate on bare
  # names rather than verbose rows. Capture the listing through the transaction
  # runner so a large archive remains interruptible and a non-zero `tar -tzf`
  # exit (corrupt archive, IO error) is surfaced as unsafe.
  #
  # `*../*` covers `..` as any path component (`a/../b`, `./../b`);
  # `*/..` and `..` catch the trailing/standalone cases. Together they
  # reject every shell-glob representation of a traversal segment without
  # a full path canonicalizer.
  _install_run_to_file "$names" tar -tzf "$archive" 2>/dev/null || return 1
  while IFS= read -r entry; do
    case "$entry" in
      /*) return 1 ;;
      *../*) return 1 ;;
      */..) return 1 ;;
      ..) return 1 ;;
    esac
  done <"$names"

  # Pass 2: reject symlink and hardlink entries. `tar -tzf` lists only
  # names, so a verbose listing is required to see entry types. The
  # leading character of the mode column is `l` for symlinks and `h` for
  # hardlinks on both GNU and BSD tar; reject either.
  _install_run_to_file "$verbose" tar -tzvf "$archive" 2>/dev/null || return 1
  while IFS= read -r entry; do
    case "${entry:0:1}" in
      l | h) return 1 ;;
    esac
  done <"$verbose"
  return 0
}

_install_release_fail() {
  local kind="$1" message="$2"

  # Cancellation is control flow, not an artifact or network diagnosis. The
  # transaction owns the final signal status and scratch cleanup, so do not
  # emit a misleading corruption/download error after its latch is set.
  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    return "$_shdeps_tx_signal"
  fi
  # The surrounding transaction owns scratch cleanup. This helper only records
  # a stable failure class and renders the user-facing diagnostic.
  _SHDEPS_RELEASE_FAILURE_KIND="$kind"
  _error "$message"
  return 1
}

_install_transaction_restore_trap() {
  local saved="$1" signal="$2"

  if [[ -n "$saved" ]]; then
    eval "$saved"
  else
    trap - "$signal"
  fi
}

_install_transaction_restore_traps() {
  _install_transaction_restore_trap "$_shdeps_tx_saved_hup" HUP
  _install_transaction_restore_trap "$_shdeps_tx_saved_int" INT
  _install_transaction_restore_trap "$_shdeps_tx_saved_term" TERM
  if [[ "$_SHDEPS_INSTALL_EXECUTED" -eq 1 ]]; then
    trap - EXIT
  fi
  if [[ "${_shdeps_tx_monitor:-0}" -eq 1 ]]; then
    set -m
  fi
}

_install_transaction_prepare_paths() {
  local mode="$1" parent tmp_root candidate attempt=0

  parent=$(dirname "$SHDEPS_DIR")
  tmp_root=${TMPDIR:-/tmp}
  while ((attempt < 20)); do
    candidate="$$.$RANDOM.$RANDOM"
    _shdeps_tx_staging="$parent/.shdeps-install.$candidate"
    _shdeps_tx_backup="$_shdeps_tx_staging.backup"
    _shdeps_tx_staging_token="staging.$candidate"
    _shdeps_tx_release_token="release.$candidate"
    if [[ "$mode" == release ]]; then
      _shdeps_tx_release_path="$tmp_root/shdeps-release.$candidate"
    else
      _shdeps_tx_release_path=""
    fi
    if [[ ! -e "$_shdeps_tx_staging" && ! -L "$_shdeps_tx_staging" &&
      ! -e "$_shdeps_tx_backup" && ! -L "$_shdeps_tx_backup" ]]; then
      [[ -z "$_shdeps_tx_release_path" ]] && return 0
      [[ ! -e "$_shdeps_tx_release_path" && ! -L "$_shdeps_tx_release_path" ]] && return 0
    fi
    attempt=$((attempt + 1))
  done
  _error "failed to reserve unique shdeps install paths"
  return 1
}

_install_private_directory() {
  local path="$1" owner_token="${2:-}" marker="$1/.shdeps-install-owner"

  # These paths hold downloaded executables and the next live install. Match
  # `mktemp -d` privacy even when the caller has an unusually permissive umask;
  # the subshell keeps the caller's umask unchanged in sourced mode. An optional
  # marker makes transaction ownership survive directory moves and prevents an
  # immediately reused inode from transferring cleanup ownership. A staged
  # bundle retains the marker as managed-install metadata, letting its next
  # replacement validate the same token after moving it to the backup path.
  (
    umask 077
    mkdir "$path" || exit $?
    [[ -n "$owner_token" ]] || exit 0
    if printf '%s\n' "$owner_token" >"$marker"; then
      exit 0
    fi
    rm -f -- "$marker" 2>/dev/null || true
    rmdir "$path" 2>/dev/null || true
    exit 1
  )
}

_install_directory_identity() {
  local path="$1" expected_token="${2:-}" identity=""
  local marker="$1/.shdeps-install-owner" owner_token=""

  identity=$(stat -c '%d:%i' "$path" 2>/dev/null) ||
    identity=$(stat -f '%d:%i' "$path" 2>/dev/null) || return 1
  [[ -n "$identity" ]] || return 1
  if [[ -n "$expected_token" || -e "$marker" || -L "$marker" ]]; then
    [[ -f "$marker" && ! -L "$marker" ]] || return 1
    IFS= read -r owner_token <"$marker" || return 1
    [[ -n "$owner_token" && "$owner_token" != *:* ]] || return 1
    [[ -z "$expected_token" || "$owner_token" == "$expected_token" ]] || return 1
    identity="$identity:$owner_token"
  fi
  printf '%s\n' "$identity"
}

_install_identity_matches() {
  local path="$1" expected="$2" actual=""

  [[ -n "$expected" && (-e "$path" || -L "$path") ]] || return 1
  actual=$(_install_directory_identity "$path" 2>/dev/null) || return 1
  [[ "$actual" == "$expected" ]]
}

_install_remove_unidentified_directory() {
  local path="$1" label="$2" owner_token="${3:-}"
  local marker="$1/.shdeps-install-owner" actual_token=""

  # Creation succeeded but stat did not, so recursive cleanup has no identity
  # proof. A matching creation token is still sufficient to remove our marker;
  # after that, only remove the empty pathname. Foreign contents or a replaced
  # marker retain the directory and become a hard cleanup error.
  if [[ -n "$owner_token" ]]; then
    if [[ ! -f "$marker" || -L "$marker" ]] ||
      ! IFS= read -r actual_token <"$marker" ||
      [[ "$actual_token" != "$owner_token" ]] ||
      ! rm -f -- "$marker"; then
      _error "failed to remove unidentified $label; retained at $path"
      return 1
    fi
  fi
  if rmdir "$path"; then
    return 0
  fi
  _error "failed to remove unidentified $label; retained at $path"
  return 1
}

_install_transaction_reconcile() {
  if [[ -n "$_shdeps_tx_staging_identity" ]] &&
    _install_identity_matches "$SHDEPS_DIR" "$_shdeps_tx_staging_identity"; then
    # The staged inode reached the live path. Keep owning that exact inode even
    # if a post-move validation fails, so cleanup can remove it and restore the
    # old install instead of exposing an incomplete executable tree.
    if _is_bundle_dir "$SHDEPS_DIR"; then
      _shdeps_tx_state=ACTIVE
      _shdeps_tx_staging_owned=0
    else
      _shdeps_tx_staging="$SHDEPS_DIR"
      _shdeps_tx_staging_owned=1
      _shdeps_tx_state=STAGING_MOVED
    fi
    return 0
  fi

  if _install_identity_matches "$_shdeps_tx_backup" "$_shdeps_tx_old_identity"; then
    _shdeps_tx_backup_owned=1
    _shdeps_tx_backup_unverified=0
  elif [[ "$_shdeps_tx_backup_owned" -eq 1 ||
    (-n "$_shdeps_tx_old_identity" &&
    ! -e "$SHDEPS_DIR" && ! -L "$SHDEPS_DIR" &&
    (-e "$_shdeps_tx_backup" || -L "$_shdeps_tx_backup")) ]]; then
    # Ownership follows the inode, not the randomized pathname. A concurrent
    # replacement must never become eligible for transaction cleanup. However,
    # an existing path that cannot be identified may still contain the only old
    # install. This also covers the first identity check after a successful move:
    # the live path disappearing while the backup appears is enough to retain a
    # hard recovery condition, but never enough to delete the unverified path.
    _shdeps_tx_backup_owned=0
    _shdeps_tx_backup_unverified=1
    _shdeps_tx_old_recovery="$_shdeps_tx_backup"
  fi
  if [[ "$_shdeps_tx_backup_unverified" -eq 1 ]]; then
    _shdeps_tx_state=BACKUP_UNVERIFIED
  elif [[ "$_shdeps_tx_backup_owned" -eq 1 && ! -e "$SHDEPS_DIR" && ! -L "$SHDEPS_DIR" ]]; then
    _shdeps_tx_state=OLD_MOVED
  else
    _shdeps_tx_state=PREPARED
  fi
}

_install_transaction_recover_backup_race() {
  local nested="$_shdeps_tx_backup/${SHDEPS_DIR##*/}"
  local moved_nested="$SHDEPS_DIR/${nested##*/}" rc=0

  _shdeps_tx_backup_owned=0
  _shdeps_tx_backup_unverified=0
  _shdeps_tx_old_recovery=""
  if _install_identity_matches "$SHDEPS_DIR" "$_shdeps_tx_old_identity"; then
    _error "shdeps backup path appeared during activation; old install remains at $SHDEPS_DIR"
    return 1
  fi
  if _install_identity_matches "$_shdeps_tx_backup" "$_shdeps_tx_old_identity"; then
    _shdeps_tx_backup_owned=1
    _shdeps_tx_old_recovery="$_shdeps_tx_backup"
    _error "shdeps backup path appeared during activation; recoverable old install retained at $_shdeps_tx_backup"
    return 1
  fi
  if ! _install_identity_matches "$nested" "$_shdeps_tx_old_identity"; then
    _error "shdeps backup path appeared during activation; expected old install identity could not be located"
    return 1
  fi

  _shdeps_tx_old_recovery="$nested"
  if [[ ! -e "$SHDEPS_DIR" && ! -L "$SHDEPS_DIR" ]]; then
    if mv "$nested" "$SHDEPS_DIR"; then rc=0; else rc=$?; fi
    if _install_identity_matches "$SHDEPS_DIR" "$_shdeps_tx_old_identity"; then
      _shdeps_tx_old_recovery=""
      _error "shdeps backup path appeared during activation; old install restored and foreign backup preserved at $_shdeps_tx_backup"
      return 1
    fi
    if _install_identity_matches "$moved_nested" "$_shdeps_tx_old_identity"; then
      _shdeps_tx_old_recovery="$moved_nested"
    elif ! _install_identity_matches "$nested" "$_shdeps_tx_old_identity"; then
      _shdeps_tx_old_recovery=""
    fi
  fi

  if [[ -n "$_shdeps_tx_old_recovery" ]]; then
    _error "shdeps backup path appeared during activation; recoverable old install retained at $_shdeps_tx_old_recovery"
  else
    _error "shdeps backup path appeared during activation; old install recovery could not be verified (mv status $rc)"
  fi
  return 1
}

_install_transaction_track_staging_after_move() {
  local nested="$SHDEPS_DIR/${_shdeps_tx_staging##*/}"

  if _install_identity_matches "$_shdeps_tx_staging" "$_shdeps_tx_staging_identity"; then
    _shdeps_tx_staging_owned=1
    return 0
  fi
  if _install_identity_matches "$nested" "$_shdeps_tx_staging_identity"; then
    _shdeps_tx_staging="$nested"
    _shdeps_tx_staging_owned=1
    return 0
  fi
  _shdeps_tx_staging_owned=0
  return 1
}

# Cleanup commands are allowed after the signal latch is set. They still use an
# exact child so a repeated signal cannot strand an unowned removal process.
_install_transaction_cleanup_run() {
  local rc=0 shielded=0

  if [[ "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
    # The operation result is already latched. Ignore later handled signals for
    # this exact cleanup child so repeated terminal-group delivery cannot turn a
    # recoverable partial removal into a permanent scratch leak.
    trap '' HUP INT TERM
    shielded=1
  fi
  "$@" &
  _shdeps_tx_child=$!
  while :; do
    if wait "$_shdeps_tx_child"; then
      rc=0
      _shdeps_tx_child=""
      break
    else
      rc=$?
      if _install_transaction_job_running "$_shdeps_tx_child"; then
        if [[ "$shielded" -eq 0 && "${_shdeps_tx_signal:-0}" -ne 0 ]]; then
          _install_transaction_cancel_child
          break
        fi
        # Caller-owned traps such as WINCH or USR1 can interrupt this wait too.
        # Cleanup still owns the live child, so keep waiting instead of killing
        # a healthy removal and converting a benign signal into a scratch leak.
        continue
      fi
      # The child may have completed while a caller trap ran. Collect its
      # retained status rather than returning the unrelated signal's 128+N.
      if wait "$_shdeps_tx_child"; then rc=0; else rc=$?; fi
      _shdeps_tx_child=""
      break
    fi
  done
  [[ "$shielded" -eq 0 ]] || _install_transaction_install_signal_traps
  return "$rc"
}

_install_transaction_remove_owned() {
  local path="$1" label="$2" expected_identity="${3:-}" runner="${4:-cleanup}"

  if [[ -z "$expected_identity" ]]; then
    _error "refusing to remove $label at $path because its ownership identity is missing"
    return 1
  fi
  [[ -e "$path" || -L "$path" ]] || return 0
  if ! _install_identity_matches "$path" "$expected_identity"; then
    _error "refusing to remove $label at $path because its identity changed"
    return 1
  fi
  if [[ "$runner" == normal ]]; then
    _install_run rm -rf -- "$path" || {
      _error "failed to remove $label; retained at $path"
      return 1
    }
  elif ! _install_transaction_cleanup_run rm -rf -- "$path"; then
    # The first transaction signal may interrupt the cleanup child itself.
    # Once latched, later parent-only delivery no longer forwards to children,
    # so revalidate exact ownership and make one controlled retry rather than
    # permanently marking a partially removed directory as cleaned.
    if [[ ! -e "$path" && ! -L "$path" ]]; then
      return 0
    fi
    if [[ "${_shdeps_tx_signal:-0}" -eq 0 ]] ||
      ! _install_identity_matches "$path" "$expected_identity" ||
      ! _install_transaction_cleanup_run rm -rf -- "$path"; then
      _error "failed to remove $label; retained at $path"
      return 1
    fi
  fi
  if [[ -e "$path" || -L "$path" ]]; then
    _error "failed to remove $label; retained at $path"
    return 1
  fi
}

_install_transaction_restore_old() {
  local nested="$SHDEPS_DIR/${_shdeps_tx_backup##*/}" rc=0

  if [[ -e "$SHDEPS_DIR" || -L "$SHDEPS_DIR" ]]; then
    _error "cannot restore shdeps backup because $SHDEPS_DIR unexpectedly exists; recoverable backup retained at $_shdeps_tx_backup"
    return 1
  fi
  if mv "$_shdeps_tx_backup" "$SHDEPS_DIR"; then rc=0; else rc=$?; fi
  if _install_identity_matches "$SHDEPS_DIR" "$_shdeps_tx_old_identity"; then
    _shdeps_tx_backup_owned=0
    _shdeps_tx_old_recovery=""
    return 0
  fi
  if _install_identity_matches "$nested" "$_shdeps_tx_old_identity"; then
    _shdeps_tx_backup_owned=0
    _shdeps_tx_old_recovery="$nested"
    _error "failed to restore shdeps because the destination appeared; recoverable old install retained at $nested"
    return 1
  fi
  if _install_identity_matches "$_shdeps_tx_backup" "$_shdeps_tx_old_identity"; then
    _shdeps_tx_backup_owned=1
    _shdeps_tx_old_recovery="$_shdeps_tx_backup"
    _error "failed to restore shdeps (mv status $rc); recoverable backup retained at $_shdeps_tx_backup"
    return 1
  fi
  _shdeps_tx_backup_owned=0
  _shdeps_tx_old_recovery=""
  _error "failed to restore shdeps (mv status $rc); expected old install identity could not be located"
  return 1
}

_install_transaction_cleanup() {
  local failed=0

  [[ "${_shdeps_tx_cleanup_done:-0}" -eq 0 ]] || return "$_shdeps_tx_cleanup_failed"
  _install_transaction_cancel_child
  _install_transaction_reconcile

  case "$_shdeps_tx_state" in
    ACTIVE)
      if [[ "$_shdeps_tx_backup_owned" -eq 1 ]] &&
        ! _install_transaction_remove_owned "$_shdeps_tx_backup" "superseded shdeps backup" "$_shdeps_tx_old_identity"; then
        failed=1
      else
        _shdeps_tx_backup_owned=0
      fi
      ;;
    OLD_MOVED)
      if ! _install_transaction_restore_old; then
        failed=1
      fi
      ;;
    BACKUP_UNVERIFIED)
      _error "cannot verify old shdeps install; expected recovery path at $_shdeps_tx_old_recovery"
      failed=1
      ;;
    STAGING_MOVED)
      if [[ "$_shdeps_tx_staging_owned" -eq 1 ]] &&
        ! _install_transaction_remove_owned "$SHDEPS_DIR" \
          "incomplete shdeps install" "$_shdeps_tx_staging_identity"; then
        failed=1
      else
        _shdeps_tx_staging_owned=0
        if [[ "$_shdeps_tx_backup_owned" -eq 1 ]] &&
          ! _install_transaction_restore_old; then
          failed=1
        fi
      fi
      ;;
    PREPARED)
      if [[ "$_shdeps_tx_backup_owned" -eq 1 ]] &&
        [[ -e "$_shdeps_tx_backup" || -L "$_shdeps_tx_backup" ]]; then
        _error "cannot restore shdeps backup because $SHDEPS_DIR appeared before activation; recoverable backup retained at $_shdeps_tx_backup"
        failed=1
      fi
      if [[ -n "$_shdeps_tx_old_recovery" ]] &&
        _install_identity_matches "$_shdeps_tx_old_recovery" "$_shdeps_tx_old_identity"; then
        _error "recoverable old install retained at $_shdeps_tx_old_recovery"
        failed=1
      fi
      ;;
  esac

  if [[ "$_shdeps_tx_staging_owned" -eq 1 ]] &&
    ! _install_transaction_remove_owned "$_shdeps_tx_staging" "shdeps install staging" "$_shdeps_tx_staging_identity"; then
    failed=1
  fi
  if [[ "$_shdeps_tx_release_owned" -eq 1 ]] &&
    ! _install_transaction_remove_owned "$_shdeps_tx_release_path" "shdeps release scratch" "$_shdeps_tx_release_identity"; then
    failed=1
  fi
  _shdeps_tx_cleanup_failed="$failed"
  if [[ "$failed" -eq 0 ]]; then
    _shdeps_tx_cleanup_done=1
    _shdeps_tx_state=DONE
  fi
  return "$failed"
}

_install_transaction_exit() {
  local status="$1" cleanup_rc=0

  _install_transaction_cleanup || cleanup_rc=$?
  _install_transaction_restore_traps
  [[ "$cleanup_rc" -eq 0 ]] || status=125
  exit "$status"
}

_install_transaction() {
  local mode="$1" src_dir="${2:-}" rc=0 cleanup_rc=0
  local _shdeps_tx_active=1 _shdeps_tx_signal=0 _shdeps_tx_child=""
  local _shdeps_tx_state=PREPARED _shdeps_tx_staging="" _shdeps_tx_backup=""
  local _shdeps_tx_release_path="" _shdeps_tx_release_identity=""
  local _shdeps_tx_staging_identity="" _shdeps_tx_old_identity=""
  local _shdeps_tx_staging_token="" _shdeps_tx_release_token=""
  local _shdeps_tx_old_recovery="" _shdeps_tx_monitor=0
  local _shdeps_tx_staging_owned=0 _shdeps_tx_backup_owned=0
  local _shdeps_tx_backup_unverified=0
  local _shdeps_tx_release_owned=0 _shdeps_tx_cleanup_done=0
  local _shdeps_tx_cleanup_failed=0
  local _shdeps_tx_saved_hup _shdeps_tx_saved_int _shdeps_tx_saved_term

  _install_transaction_prepare_paths "$mode" || return 1
  case "$-" in
    *m*)
      _shdeps_tx_monitor=1
      # Background children are an implementation detail. Temporarily disabling
      # monitor mode prevents Bash from printing job completion records into a
      # sourced caller's output; exact prior state is restored after reaping.
      set +m
      ;;
  esac
  _shdeps_tx_saved_hup=$(trap -p HUP)
  _shdeps_tx_saved_int=$(trap -p INT)
  _shdeps_tx_saved_term=$(trap -p TERM)
  _install_transaction_install_signal_traps
  if [[ "$_SHDEPS_INSTALL_EXECUTED" -eq 1 ]]; then
    trap '_install_transaction_exit "$?"' EXIT
  fi

  if [[ "$mode" == release ]]; then
    _install_release_core || rc=$?
  else
    _install_bundle_core "$src_dir" || rc=$?
  fi
  _install_transaction_cleanup || cleanup_rc=$?
  if [[ "$cleanup_rc" -ne 0 ]]; then
    # An unremoved transaction-owned path is a hard operational failure, even
    # when cleanup began because an earlier signal was latched. Returning the
    # signal would falsely tell callers that rollback completed successfully.
    rc=125
  fi
  _install_transaction_restore_traps
  # Sample the latch only after teardown. A signal delivered after cleanup but
  # before its transaction trap is restored must still be the operation result;
  # once the caller's trap is restored, subsequent delivery belongs to it.
  if [[ "$cleanup_rc" -eq 0 && "$_shdeps_tx_signal" -ne 0 ]]; then
    rc=$_shdeps_tx_signal
  fi
  return "$rc"
}

_install_bundle_core() {
  local src_dir="$1" parent name rc=0
  local -a files dirs

  if [[ -e "$SHDEPS_DIR" || -L "$SHDEPS_DIR" ]]; then
    if [[ -d "$SHDEPS_DIR/.git" ]]; then
      if ! _release_can_replace_source_checkout "$SHDEPS_DIR"; then
        return 1
      fi
    elif ! _is_release_install_dir "$SHDEPS_DIR"; then
      _error "$SHDEPS_DIR exists but is not a shdeps release install"
      return 1
    fi
  fi

  parent=$(dirname "$SHDEPS_DIR")
  if ! mkdir -p "$parent"; then
    _error "failed to create shdeps install parent at $parent"
    return 1
  fi
  # Claim the randomized pathname before allocation. Until exact identity is
  # captured, cleanup may only report and retain a leftover path; it cannot
  # recursively remove one. This closes the marker-publication interruption
  # window without transferring deletion rights to a concurrent replacement.
  _shdeps_tx_staging_owned=1
  if ! _install_private_directory "$_shdeps_tx_staging" "$_shdeps_tx_staging_token"; then
    if [[ ! -e "$_shdeps_tx_staging" && ! -L "$_shdeps_tx_staging" ]]; then
      _shdeps_tx_staging_owned=0
    fi
    _error "failed to create shdeps install staging at $_shdeps_tx_staging"
    return 1
  fi
  _shdeps_tx_staging_identity=$(
    _install_directory_identity "$_shdeps_tx_staging" "$_shdeps_tx_staging_token"
  ) || {
    if _install_remove_unidentified_directory \
      "$_shdeps_tx_staging" "shdeps install staging" "$_shdeps_tx_staging_token"; then
      _shdeps_tx_staging_owned=0
    fi
    _error "failed to identify shdeps install staging"
    return 1
  }

  # Release archives are already verified before users run their bundled
  # installer, but filesystem activation can still fail. Copy into a sibling
  # staging directory first so an interrupted or full-disk install does not
  # leave SHDEPS_DIR looking usable while missing the wrapper or metadata.
  #
  # Keep files and directories in two portable cp batches so each potentially
  # long copy remains one exact owned child without paying one fork/wait per
  # bundle entry. Separate batches preserve the prior -p versus -R semantics.
  files=(
    "$src_dir/shdeps"
    "$src_dir/shdeps.sh"
    "$src_dir/install.sh"
    "$src_dir/.shdeps-install.json"
  )
  for name in README.md LICENSE; do
    [[ -f "$src_dir/$name" ]] && files[${#files[@]}]="$src_dir/$name"
  done
  dirs=()
  for name in man completions lua; do
    [[ -d "$src_dir/$name" ]] && dirs[${#dirs[@]}]="$src_dir/$name"
  done
  _install_run cp -p "${files[@]}" "$_shdeps_tx_staging/" || return $?
  if ((${#dirs[@]} > 0)); then
    _install_run cp -R "${dirs[@]}" "$_shdeps_tx_staging/" || return $?
  fi

  if ! _install_identity_matches "$_shdeps_tx_staging" "$_shdeps_tx_staging_identity"; then
    _error "shdeps install staging identity changed during copy"
    return 1
  fi
  if ! _is_bundle_dir "$_shdeps_tx_staging"; then
    _error "staged shdeps bundle is incomplete"
    return 1
  fi

  if [[ -e "$SHDEPS_DIR" || -L "$SHDEPS_DIR" ]]; then
    if [[ -e "$_shdeps_tx_backup" || -L "$_shdeps_tx_backup" ]]; then
      _error "shdeps backup path appeared before activation: $_shdeps_tx_backup"
      return 1
    fi
    _shdeps_tx_old_identity=$(_install_directory_identity "$SHDEPS_DIR") || {
      _error "failed to identify existing shdeps install before activation"
      return 1
    }
    if mv "$SHDEPS_DIR" "$_shdeps_tx_backup"; then rc=0; else rc=$?; fi
    _install_transaction_reconcile
    if [[ "$_shdeps_tx_signal" -ne 0 ]]; then return "$_shdeps_tx_signal"; fi
    if [[ "$rc" -ne 0 ]] &&
      _install_identity_matches "$SHDEPS_DIR" "$_shdeps_tx_old_identity"; then
      _error "failed to move existing shdeps install to backup (mv status $rc)"
      return "$rc"
    fi
    if [[ "$_shdeps_tx_state" != OLD_MOVED ]]; then
      _install_transaction_recover_backup_race || true
      return 1
    fi
    if [[ "$rc" -ne 0 ]]; then return "$rc"; fi
  fi

  if [[ -e "$SHDEPS_DIR" || -L "$SHDEPS_DIR" ]]; then
    if [[ "$_shdeps_tx_backup_owned" -eq 1 ]]; then
      _error "$SHDEPS_DIR appeared before activation; recoverable backup retained at $_shdeps_tx_backup"
    else
      _error "$SHDEPS_DIR appeared before activation; refusing to replace it"
    fi
    return 1
  fi
  if mv "$_shdeps_tx_staging" "$SHDEPS_DIR"; then rc=0; else rc=$?; fi
  _install_transaction_reconcile
  if [[ "$_shdeps_tx_state" != ACTIVE ]]; then
    _install_transaction_track_staging_after_move || true
  fi
  if [[ "$_shdeps_tx_signal" -ne 0 ]]; then return "$_shdeps_tx_signal"; fi
  if [[ "$_shdeps_tx_state" != ACTIVE ]]; then
    if [[ -e "$SHDEPS_DIR" || -L "$SHDEPS_DIR" ]]; then
      if [[ "$_shdeps_tx_backup_owned" -eq 1 ]]; then
        _error "$SHDEPS_DIR appeared before activation; recoverable backup retained at $_shdeps_tx_backup"
      else
        _error "$SHDEPS_DIR appeared before activation; refusing to replace it"
      fi
      return 1
    fi
    [[ "$rc" -ne 0 ]] && return "$rc"
    _error "activated shdeps install failed identity or bundle validation"
    return 1
  fi

  if [[ "$_shdeps_tx_backup_owned" -eq 1 ]]; then
    _install_transaction_remove_owned "$_shdeps_tx_backup" "superseded shdeps backup" "$_shdeps_tx_old_identity" normal || return 1
    _shdeps_tx_backup_owned=0
  fi
  _info "shdeps: installed"
}

_install_bundle() {
  _install_transaction bundle "$1"
}

_release_can_replace_source_checkout() {
  local dir="$1" origin="" expected

  expected=$(_repo_slug)
  origin=$(git -C "$dir" config --get remote.origin.url 2>/dev/null || true)
  if [[ -z "$origin" || "$(_repo_url_slug "$origin")" != "$expected" ]]; then
    _error "$dir is a git checkout for ${origin:-unknown}; refusing release migration"
    return 1
  fi

  # Release migration is meant for fleet-owned source installs left behind by
  # the Bash-to-Rust transition. Preserve real edits, but ignore the small set
  # of artifacts install.sh itself may have created while activating a managed
  # checkout: release/source metadata, the root CLI link, and Cargo build
  # output. Treating those as user dirt traps old fleet installs on the source
  # path and makes fresh machines need Rust for no good reason.
  if _source_checkout_has_user_changes "$dir"; then
    _error "$dir is a dirty git checkout; refusing release migration"
    return 1
  fi
}

_source_checkout_has_user_changes() {
  local dir="$1" line status path

  while IFS= read -r line; do
    status="${line:0:2}"
    path="${line:3}"
    case "$status:$path" in
      "??:.shdeps-install.json" | "??:shdeps" | "??:target" | "??:target/"*)
        continue
        ;;
    esac
    return 0
  done < <(git -C "$dir" status --porcelain --untracked-files=normal 2>/dev/null)

  return 1
}

_cleanup_installed_state_for_source_checkout() {
  local source_dir="$1" source_real install_real cleanup=0

  [[ -d "$source_dir/.git" ]] || return 0

  source_real=$(cd -P -- "$source_dir" 2>/dev/null && pwd) || return 0
  install_real=$(cd -P -- "$SHDEPS_DIR" 2>/dev/null && pwd) || return 0
  [[ "$source_real" != "$install_real" ]] || return 0

  # Once a developer checkout is the active implementation, any owned install
  # under SHDEPS_DIR is stale state. Release payloads are explicitly owned by
  # shdeps metadata. Clean source checkouts of the same repo are also owned:
  # they are the old fleet-managed clone shape that should not linger after a
  # real dev clone takes over. Dirty or foreign checkouts are preserved because
  # they might be someone else's work.
  if _is_release_install_dir "$SHDEPS_DIR"; then
    cleanup=1
  elif [[ -d "$SHDEPS_DIR/.git" ]] &&
    _release_can_replace_source_checkout "$SHDEPS_DIR" >/dev/null 2>&1; then
    cleanup=1
  fi
  [[ "$cleanup" -eq 1 ]] || return 0

  if ! rm -rf "$SHDEPS_DIR"; then
    _error "failed to remove stale shdeps install at $SHDEPS_DIR"
    return 1
  fi
}

_install_release_core() {
  local platform repo api_url token tmp json tag archive checksum
  local archive_url checksum_url archive_api_url checksum_api_url bundle
  local _SHDEPS_LATEST_RELEASE_TAG=""
  _SHDEPS_RELEASE_FAILURE_KIND=""
  platform=$(_release_platform) || return 1
  repo=$(_repo_slug)
  _github_token token
  tmp="$_shdeps_tx_release_path"
  _shdeps_tx_release_owned=1
  if ! _install_private_directory "$tmp" "$_shdeps_tx_release_token"; then
    if [[ ! -e "$tmp" && ! -L "$tmp" ]]; then
      _shdeps_tx_release_owned=0
    fi
    _error "failed to create release staging directory"
    return 1
  fi
  _shdeps_tx_release_identity=$(
    _install_directory_identity "$tmp" "$_shdeps_tx_release_token"
  ) || {
    if _install_remove_unidentified_directory \
      "$tmp" "shdeps release scratch" "$_shdeps_tx_release_token"; then
      _shdeps_tx_release_owned=0
    fi
    _error "failed to identify release staging directory"
    return 1
  }
  json="$tmp/release.json"

  if [[ -n "${SHDEPS_RELEASE_API_URL:-}" ]]; then
    api_url="$SHDEPS_RELEASE_API_URL"
    if ! _curl_get "$api_url" "$json" "$token"; then
      _install_release_fail "download" "failed to fetch shdeps release metadata"
      return $?
    fi

    tag=$(_json_string "$json" "tag_name")
    if [[ -z "$tag" ]]; then
      _install_release_fail "metadata" "release metadata did not contain tag_name"
      return $?
    fi

    archive="shdeps-${tag}-${platform}.tar.gz"
    checksum="${archive}.sha256"
    archive_url=$(_asset_url "$json" "$archive")
    checksum_url=$(_asset_url "$json" "$checksum")
    archive_api_url=$(_asset_api_url "$json" "$archive")
    checksum_api_url=$(_asset_api_url "$json" "$checksum")
    if [[ -z "$archive_url" || -z "$checksum_url" ]]; then
      _install_release_fail "metadata" "release $tag does not contain assets for $platform"
      return $?
    fi
  else
    # For the public default repo, resolve the latest tag through the normal
    # GitHub release redirect and construct canonical asset URLs. This avoids
    # unauthenticated API rate limits during fleet bootstrap while keeping the
    # archive/checksum contract explicit and easy to inspect.
    if ! _latest_release_tag "$repo"; then
      _install_release_fail "download" "failed to resolve latest shdeps release"
      return $?
    fi
    tag="$_SHDEPS_LATEST_RELEASE_TAG"
    archive="shdeps-${tag}-${platform}.tar.gz"
    checksum="${archive}.sha256"
    archive_url="https://github.com/$repo/releases/download/$tag/$archive"
    checksum_url="https://github.com/$repo/releases/download/$tag/$checksum"
    archive_api_url=""
    checksum_api_url=""
    # Browser download URLs for this public repo do not need API auth. Avoid
    # forwarding a caller's unrelated or expired GitHub token to github.com,
    # because a bad token should not make public bootstrap fail.
    token=""
  fi

  if ! _curl_get_release_asset "$archive_url" "$tmp/$archive" "$token" "$archive_api_url"; then
    _install_release_fail "download" "failed to download $archive"
    return $?
  fi
  if ! _curl_get_release_asset "$checksum_url" "$tmp/$checksum" "$token" "$checksum_api_url"; then
    _install_release_fail "download" "failed to download $checksum"
    return $?
  fi
  if ! _verify_checksum "$tmp" "$archive" "$checksum" >/dev/null; then
    _install_release_fail "artifact" "checksum verification failed for $archive"
    return $?
  fi

  bundle="$tmp/bundle"
  if ! mkdir -p "$bundle"; then
    _install_release_fail "artifact" "failed to create release bundle directory"
    return $?
  fi
  # Tar traversal hardening: list the archive contents before extracting
  # and refuse any entry whose path is absolute (`/foo`) or escapes the
  # destination via `..`. GNU tar's `--no-absolute-filenames` is one
  # mitigation but not portable to the BSD tar shipped on macOS; the
  # list-and-validate approach works on both. The Rust extraction path
  # (`archive.rs`) does the same check at a higher level; this is the
  # bootstrap-side equivalent for the curl-pipe install path.
  if ! _archive_entries_safe "$tmp/$archive"; then
    _install_release_fail "artifact" "refusing to extract $archive: contains absolute or traversal paths"
    return $?
  fi
  if ! _install_run tar -xzf "$tmp/$archive" -C "$bundle"; then
    _install_release_fail "artifact" "failed to extract $archive"
    return $?
  fi
  if ! _install_bundle_core "$bundle"; then
    _install_release_fail "artifact" "failed to install $archive"
    return $?
  fi
}

_install_release() {
  _install_transaction release
}

_ensure_source_checkout_binary() {
  local shdeps_dir="$1"
  local head=""

  if [[ -d "$shdeps_dir/.git" ]]; then
    head=$(git -C "$shdeps_dir" rev-parse --short=8 HEAD 2>/dev/null || true)
  fi

  # A source checkout can move forward without replacing its already-built
  # binary: `dot update` sources install.sh, then shdeps self-update may only
  # run `git pull`. Treat the build hash as the freshness marker so a pulled
  # Rust checkout cannot keep delegating to an older executable indefinitely.
  if _source_checkout_binary_matches_head "$shdeps_dir/shdeps" "$head"; then
    return 0
  fi

  if _source_checkout_binary_matches_head "$shdeps_dir/target/release/shdeps" "$head"; then
    ln -sf "target/release/shdeps" "$shdeps_dir/shdeps"
    return 0
  fi

  if _source_checkout_binary_matches_head "$shdeps_dir/target/debug/shdeps" "$head"; then
    # Developer activation often happens immediately after `cargo build`.
    # Accepting that existing debug binary keeps local bootstrap snappy; fresh
    # clones with no binary still build release below, and fleet installs use
    # release archives rather than this source-checkout path.
    ln -sf "target/debug/shdeps" "$shdeps_dir/shdeps"
    return 0
  fi

  if [[ ! -f "$shdeps_dir/Cargo.toml" ]]; then
    _error "$shdeps_dir is missing a Rust binary and Cargo.toml"
    return 1
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    _error "cargo is required to activate source checkout installs"
    return 1
  fi

  # Source-checkout mode is now a developer/explicit-repo path: normal fleet
  # installs use prebuilt release archives and do not need Rust. When a caller
  # explicitly asks install.sh to use a checkout, build the Rust binary before
  # sourcing `shdeps.sh`; otherwise the wrapper could accidentally delegate to
  # an older `shdeps` on PATH or fail after the clone already succeeded.
  # Force Cargo's target dir back under the checkout. A user's ambient
  # CARGO_TARGET_DIR is useful for normal development, but bootstrap activation
  # needs one predictable sibling binary so the sourceable wrapper and the
  # ~/.local/bin link cannot point at different builds.
  if ! (cd "$shdeps_dir" && CARGO_TARGET_DIR="$shdeps_dir/target" cargo build --release --locked); then
    _error "failed to build shdeps source checkout"
    return 1
  fi
  if [[ ! -x "$shdeps_dir/target/release/shdeps" ]]; then
    _error "source checkout build did not produce target/release/shdeps"
    return 1
  fi
  ln -sf "target/release/shdeps" "$shdeps_dir/shdeps"
}

_source_checkout_binary_matches_head() {
  local binary="$1" head="$2" version=""

  [[ -x "$binary" ]] || return 1
  if [[ -z "$head" ]]; then
    # Non-git source fixtures and pre-Rust installs cannot provide a durable
    # commit identity. In that fallback shape, any executable is still better
    # than rejecting a usable install during bootstrap.
    return 0
  fi

  version=$("$binary" version 2>/dev/null || true)
  case "$version" in
    *"$head"*) return 0 ;;
    *) return 1 ;;
  esac
}

_bootstrap_self_update() {
  local shdeps_dir="$1"

  if declare -f shdeps_self_update &>/dev/null; then
    # The Rust public wrapper intentionally has a clean no-arg CLI surface, so
    # pass the bootstrap-selected checkout through the environment.
    local SHDEPS_DIR="$shdeps_dir"
    export SHDEPS_DIR
    shdeps_self_update
  fi
}

# Symlink CLI into PATH and link man page + shell completions.
_setup_links() {
  local shdeps_dir="$1"
  local cli=""

  if [[ -x "$shdeps_dir/shdeps" ]]; then
    cli="$shdeps_dir/shdeps"
  elif [[ -x "$shdeps_dir/bin/shdeps" ]]; then
    cli="$shdeps_dir/bin/shdeps"
  fi

  if [[ -n "$cli" ]]; then
    mkdir -p "$(dirname "$SHDEPS_BIN")" || return 1
    ln -sf "$cli" "$SHDEPS_BIN" || return 1
  fi

  if declare -f shdeps_link_extras &>/dev/null; then
    shdeps_link_extras "shdeps" "$shdeps_dir" || return 1
  fi
}

_source_installed_library_for_extras() {
  local shdeps_dir="$1"

  if [[ ! -f "$shdeps_dir/shdeps.sh" ]]; then
    return 0
  fi

  # `install.sh` is the bootstrap script users run before shdeps is installed,
  # so it must stay usable with stock macOS Bash 3.2. Release installs can
  # still activate the Rust binary and CLI symlink under Bash 3.2; they only
  # skip optional extras linking that depends on sourcing shdeps.sh.
  if ! _bash_supports_sourceable_wrapper; then
    return 0
  fi

  # shellcheck source=/dev/null
  . "$shdeps_dir/shdeps.sh"
}

_activate_installed_tree() {
  local shdeps_dir="$1"

  _source_installed_library_for_extras "$shdeps_dir" || return $?
  _setup_links "$shdeps_dir"
}

# ---------------------------------------------------------------------------
# Install / update
# ---------------------------------------------------------------------------

_install() {
  local script_dir rc=0
  script_dir=$(_script_dir) || return 1

  if _is_bundle_dir "$script_dir"; then
    _install_bundle "$script_dir" || rc=$?
    [[ "$rc" -eq 0 ]] || return "$rc"
    _activate_installed_tree "$SHDEPS_DIR" || return $?
    return
  fi

  if _uses_default_repo_slug && ! _is_source_checkout_dir "$script_dir"; then
    _install_release || rc=$?
    [[ "$rc" -eq 0 ]] || return "$rc"
    _activate_installed_tree "$SHDEPS_DIR" || return $?
    return
  fi

  _check_source_prereqs || return 1

  if [[ -d "$SHDEPS_DIR/.git" ]]; then
    # Already installed — pull latest if clean
    if [[ -n "$(git -C "$SHDEPS_DIR" status --porcelain --untracked-files=normal 2>/dev/null)" ]]; then
      _info "shdeps: dirty working tree, skipping update"
    elif git -C "$SHDEPS_DIR" pull --ff-only --quiet; then
      _info "shdeps: updated"
    else
      _error "shdeps: update failed (git pull --ff-only failed)"
      return 1
    fi
  elif _is_release_install_dir "$SHDEPS_DIR"; then
    # Direct source installs are explicit developer/source mode. If a managed
    # release payload is present, clean it before cloning the source install so
    # the selected implementation has a single owner on disk.
    if ! rm -rf "$SHDEPS_DIR"; then
      _error "failed to remove stale shdeps release install at $SHDEPS_DIR"
      return 1
    fi
    _info "shdeps: cloning to $SHDEPS_DIR..."
    git clone --depth 1 "$SHDEPS_REPO" "$SHDEPS_DIR" || return 1
    _info "shdeps: installed"
  elif [[ -d "$SHDEPS_DIR" ]]; then
    _error "$SHDEPS_DIR exists but is not a git repo"
    return 1
  else
    _info "shdeps: cloning to $SHDEPS_DIR..."
    git clone --depth 1 "$SHDEPS_REPO" "$SHDEPS_DIR" || return 1
    _info "shdeps: installed"
  fi

  _ensure_source_checkout_binary "$SHDEPS_DIR" || return 1

  # Source the library and set up all symlinks (CLI, man, completions).
  _activate_installed_tree "$SHDEPS_DIR" || return $?

  # Hint if the bin directory isn't on PATH
  local bin_dir
  bin_dir=$(dirname "$SHDEPS_BIN")
  case ":$PATH:" in
    *":${bin_dir}:"*) ;;
    *)
      _info ""
      _info "Add $bin_dir to your PATH if it isn't already:"
      _info "  export PATH=\"${bin_dir}:\$PATH\""
      ;;
  esac
}

# ---------------------------------------------------------------------------
# Bootstrap — source shdeps into the caller
# ---------------------------------------------------------------------------
# Designed to be sourced: `. /path/to/install.sh --bootstrap`
#
# Finds shdeps.sh, sources it, and symlinks the CLI. Source checkouts still
# self-update during bootstrap; release installs stay local unless forced so
# callers are not blocked on GitHub before they can render their own UI.
# Clients set env vars (SHDEPS_CONF_DIR, SHDEPS_HOOKS_DIR, etc.) before
# sourcing.
#
# Returns 0 if shdeps is ready, 1 if bootstrap failed.

_bootstrap() {
  # Idempotent — skip if already bootstrapped
  declare -f shdeps_update &>/dev/null && return 0

  local _bs_lib="" _bs_dir="" _bs_release_install=0 _bs_rc=0
  local _dev_dir="${SHDEPS_GIT_DEV_DIR:-$HOME/git}"

  # Find shdeps.sh: installed-tree env hint → env override → dev clone →
  # installed tree → fresh install. SHDEPS_LIB usually means "do exactly this",
  # but when it points back at SHDEPS_DIR it is just an older caller's cached
  # discovery result. Route that shape through the installed-tree path so it can
  # be migrated from source checkout to release assets.
  if _bootstrap_lib_is_installed_tree; then
    if _uses_default_repo_slug && [[ -d "$SHDEPS_DIR/.git" ]]; then
      _install_release >/dev/null 2>&1 || _bs_rc=$?
      if _install_status_requires_propagation "$_bs_rc"; then return "$_bs_rc"; fi
    else
      _repair_release_if_needed "$SHDEPS_DIR" || _bs_rc=$?
      if _install_status_requires_propagation "$_bs_rc"; then return "$_bs_rc"; fi
      [[ "$_bs_rc" -eq 0 ]] || return 1
    fi
    _bs_lib="$SHDEPS_DIR/shdeps.sh"
    _bs_dir="$SHDEPS_DIR"
  elif [[ -n "${SHDEPS_LIB:-}" && -f "$SHDEPS_LIB" ]]; then
    _bs_lib="$SHDEPS_LIB"
    _bs_dir="${SHDEPS_LIB%/*}"
  elif [[ -f "$_dev_dir/shdeps/shdeps.sh" ]]; then
    _bs_lib="$_dev_dir/shdeps/shdeps.sh"
    _bs_dir="$_dev_dir/shdeps"
  elif [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    if _uses_default_repo_slug && [[ -d "$SHDEPS_DIR/.git" ]]; then
      # Older fleet machines may already have a source checkout installed in the
      # default SHDEPS_DIR. That checkout was bootstrap state, not an intentional
      # dev workspace, so prefer the release archive now that one exists. Keep
      # this opportunistic: failed downloads, dirty checkouts, or unsupported
      # platforms fall through to the source path so bootstrap still converges.
      _bs_rc=0
      _install_release >/dev/null 2>&1 || _bs_rc=$?
      if _install_status_requires_propagation "$_bs_rc"; then return "$_bs_rc"; fi
    else
      _bs_rc=0
      _repair_release_if_needed "$SHDEPS_DIR" || _bs_rc=$?
      if _install_status_requires_propagation "$_bs_rc"; then return "$_bs_rc"; fi
      [[ "$_bs_rc" -eq 0 ]] || return 1
    fi
    _bs_lib="$SHDEPS_DIR/shdeps.sh"
    _bs_dir="$SHDEPS_DIR"
  else
    # Keep a fresh install in this shell so the transaction trap owns both the
    # sourced caller signal and the exact download/copy child. `_install`
    # returns failures explicitly and therefore remains safe to call here.
    if _install >/dev/null 2>&1; then
      _bs_lib="$SHDEPS_DIR/shdeps.sh"
      _bs_dir="$SHDEPS_DIR"
    else
      _bs_rc=$?
      if _install_status_requires_propagation "$_bs_rc"; then return "$_bs_rc"; fi
      return 1
    fi
  fi

  if [[ -n "$_bs_dir" ]] && _is_release_install_dir "$_bs_dir"; then
    _bs_release_install=1
  fi

  # Bootstrap may discover a dev checkout that has not gone through install.sh
  # yet. Create the root binary link first so sourcing the Rust wrapper and
  # linking the CLI both target this checkout instead of an older PATH command.
  if [[ -n "$_bs_dir" && -d "$_bs_dir/.git" && -f "$_bs_dir/Cargo.toml" ]]; then
    _ensure_source_checkout_binary "$_bs_dir" || return 1
  fi

  # Source the library
  # shellcheck source=/dev/null
  . "$_bs_lib" || return 1

  # Pull source checkouts during bootstrap, but keep release installs local.
  # Release freshness checks can involve GitHub redirects or API calls; doing
  # that before the caller has rendered any UI caused slow/noisy `dot update`
  # startup during transient GitHub 504s. `SHDEPS_FORCE=1` still refreshes
  # release installs above, which keeps explicit "check now" behavior.
  if [[ -n "$_bs_dir" && "$_bs_release_install" -eq 0 ]]; then
    # Bootstrap is sourced into callers, so stdout belongs to the caller's
    # script. Self-update is opportunistic here and ignored on failure; keep it
    # quiet for the same reason so status chatter cannot pollute command
    # substitutions around `. install.sh --bootstrap`.
    _bootstrap_self_update "$_bs_dir" >/dev/null 2>&1 || true
  fi

  # The self-update call above may have pulled new Rust sources while the
  # currently executing binary was still the old build. Re-run the activation
  # check after the pull so `dot update` converges the checked-out source and the
  # delegated executable in the same bootstrap pass whenever this install.sh is
  # new enough to know about Rust builds.
  if [[ -n "$_bs_dir" && -d "$_bs_dir/.git" && -f "$_bs_dir/Cargo.toml" ]]; then
    _ensure_source_checkout_binary "$_bs_dir" || return 1
  fi

  # Set up symlinks (CLI, man, completions) after self-update so newly
  # pulled files (e.g. man pages, completions) are linked immediately.
  [[ -n "$_bs_dir" ]] && _setup_links "$_bs_dir"
  [[ -n "$_bs_dir" ]] && _cleanup_installed_state_for_source_checkout "$_bs_dir"
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

_uninstall() {
  local removed=0 failed=0

  # Clean up extras symlinks (man page, completions) before removing the repo
  if [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    # shellcheck source=/dev/null
    . "$SHDEPS_DIR/shdeps.sh"
    if declare -f shdeps_unlink_extras &>/dev/null; then
      shdeps_unlink_extras "shdeps"
    fi
  fi

  # Guard each removal in an `if` so a failure neither aborts the rest of the
  # uninstall under `set -e` nor passes silently — a partial uninstall must
  # surface a diagnostic and a non-zero exit so the caller knows state remains.
  if [[ -L "$SHDEPS_BIN" ]]; then
    if rm "$SHDEPS_BIN"; then
      ((removed++)) || true
    else
      _error "failed to remove CLI symlink at $SHDEPS_BIN"
      failed=1
    fi
  fi
  if [[ -d "$SHDEPS_DIR" ]]; then
    if rm -rf "$SHDEPS_DIR"; then
      ((removed++)) || true
    else
      _error "failed to remove install directory at $SHDEPS_DIR"
      failed=1
    fi
  fi
  if [[ $removed -gt 0 ]]; then
    _info "shdeps: uninstalled"
  elif [[ $failed -eq 0 ]]; then
    _info "shdeps: nothing to uninstall"
  fi
  [[ $failed -eq 0 ]]
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

# `install.sh` is normally executed as a script for the curl-pipe install
# path, but `_bootstrap` is intentionally invoked via `. install.sh
# --bootstrap` so that the caller's shell can use the freshly-installed
# helper functions afterwards. That makes the `${BASH_SOURCE}` vs `$0`
# trick unsuitable for distinguishing real invocations from unit-test
# sourcing — both go through the same sourced path. Instead, opt-out via
# the `SHDEPS_INSTALL_SH_NO_DISPATCH` env var: the test harness sets it
# before sourcing so the dispatch becomes a no-op and the helpers are
# loaded for direct exercising.
if [[ "${SHDEPS_INSTALL_SH_NO_DISPATCH:-0}" != "1" ]]; then
  case "${1:-}" in
    --uninstall) _uninstall ;;
    --bootstrap) _bootstrap ;;
    "") _install ;;
    *)
      _error "unknown argument: $1"
      _info "Usage: install.sh [--uninstall|--bootstrap]"
      exit 2
      ;;
  esac
fi
