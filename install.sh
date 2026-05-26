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
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
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

_bash_supports_legacy_library() {
  ((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3)))
}

_check_source_prereqs() {
  if ! command -v git >/dev/null 2>&1; then
    _error "git is required"
    exit 1
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
  if [[ -n "${GH_TOKEN:-}" ]]; then
    printf '%s\n' "$GH_TOKEN"
  elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
    printf '%s\n' "$GITHUB_TOKEN"
  elif command -v gh >/dev/null 2>&1; then
    gh auth token 2>/dev/null || true
  fi
}

_curl_get() {
  local url="$1" out="$2" token="${3:-}"
  local -a args=(curl -fsSL -A shdeps)
  case "$url" in
    https://api.github.com/*)
      args+=(-H "Accept: application/vnd.github+json")
      if [[ -n "$token" ]]; then
        args+=(-H "Authorization: Bearer $token")
      fi
      ;;
  esac
  "${args[@]}" -o "$out" "$url"
}

_curl_get_release_asset() {
  local browser_url="$1" out="$2" token="${3:-}" api_url="${4:-}"
  local have_api_fallback=0
  if [[ -n "$token" && -n "$api_url" ]]; then
    have_api_fallback=1
  fi

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
  curl -fsSL -A shdeps \
    -H "Accept: application/octet-stream" \
    -H "Authorization: Bearer $token" \
    -o "$out" \
    "$api_url"
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

_release_platform() {
  local os arch
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m | tr '[:upper:]' '[:lower:]')
  case "$arch" in
    amd64) arch="x86_64" ;;
    arm64) arch="aarch64" ;;
  esac

  case "$os:$arch" in
    linux:x86_64) printf '%s\n' "linux-x86_64-musl" ;;
    linux:aarch64) printf '%s\n' "linux-aarch64-musl" ;;
    darwin:x86_64) printf '%s\n' "macos-x86_64" ;;
    darwin:aarch64) printf '%s\n' "macos-aarch64" ;;
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

_installed_release_tag() {
  local dir="$1"

  _json_string "$dir/.shdeps-install.json" "tag"
}

_release_tag_newer() {
  local latest="$1" current="$2"

  [[ -n "$latest" ]] || return 1
  # When no current tag exists we cannot compute "newer than"; let the
  # caller decide whether to install. (The bootstrap refresh path already
  # short-circuits this case; this branch is here for direct callers.)
  [[ -n "$current" ]] || return 0
  [[ "$(_natural_tag_cmp "$latest" "$current")" == "gt" ]]
}

# Natural-order tag comparison shared with the Rust self-update path.
#
# Stable lexical comparison of release tags works for shdeps' own
# `YYYYMMDD-HHMMSS-<hash>` format because the timestamp prefix is
# zero-padded. It silently breaks down for any tag scheme with a numeric
# component that grows past 9 (e.g., `v1.10` vs `v1.9`, where lexical
# comparison incorrectly orders `v1.10` before `v1.9`). The Rust
# implementation in `src/self_update.rs::compare_tags` already does a
# natural sort that treats numeric runs by trimmed length and then by
# bytes, with numeric runs sorting after text runs. Mirror that here so
# bootstrap-time and runtime self-update agree on which release is newer
# for every possible tag scheme.
#
# Echoes one of `lt`, `eq`, `gt` to stdout. Stays Bash 3.2-compatible
# because `install.sh` runs from the curl-pipe path on macOS's stock
# /bin/bash.
_natural_tag_cmp() {
  local left="$1" right="$2"
  local left_pos=0 right_pos=0
  local left_len=${#left} right_len=${#right}
  local lc rc l_run r_run l_digit r_digit l_trim r_trim

  while [[ $left_pos -lt $left_len || $right_pos -lt $right_len ]]; do
    if [[ $left_pos -ge $left_len ]]; then
      echo "lt"
      return
    fi
    if [[ $right_pos -ge $right_len ]]; then
      echo "gt"
      return
    fi

    # Determine whether the next run on each side is numeric or textual.
    lc="${left:$left_pos:1}"
    rc="${right:$right_pos:1}"
    case "$lc" in [0-9]) l_digit=1 ;; *) l_digit=0 ;; esac
    case "$rc" in [0-9]) r_digit=1 ;; *) r_digit=0 ;; esac

    # Extract the maximal same-class run from each side.
    l_run=""
    if [[ $l_digit -eq 1 ]]; then
      while [[ $left_pos -lt $left_len ]]; do
        case "${left:$left_pos:1}" in [0-9]) ;; *) break ;; esac
        l_run="$l_run${left:$left_pos:1}"
        left_pos=$((left_pos + 1))
      done
    else
      while [[ $left_pos -lt $left_len ]]; do
        case "${left:$left_pos:1}" in [0-9]) break ;; esac
        l_run="$l_run${left:$left_pos:1}"
        left_pos=$((left_pos + 1))
      done
    fi
    r_run=""
    if [[ $r_digit -eq 1 ]]; then
      while [[ $right_pos -lt $right_len ]]; do
        case "${right:$right_pos:1}" in [0-9]) ;; *) break ;; esac
        r_run="$r_run${right:$right_pos:1}"
        right_pos=$((right_pos + 1))
      done
    else
      while [[ $right_pos -lt $right_len ]]; do
        case "${right:$right_pos:1}" in [0-9]) break ;; esac
        r_run="$r_run${right:$right_pos:1}"
        right_pos=$((right_pos + 1))
      done
    fi

    # Cross-class: numeric runs sort AFTER text runs so `v10` > `vbeta`.
    if [[ $l_digit -ne $r_digit ]]; then
      if [[ $l_digit -eq 1 ]]; then echo "gt"; else echo "lt"; fi
      return
    fi

    if [[ $l_digit -eq 1 ]]; then
      # Numeric: trim leading zeros, compare by length first (so 10 > 9
      # without ever fitting into a fixed-width integer), then by bytes
      # for the same-length case.
      l_trim="${l_run#"${l_run%%[!0]*}"}"
      r_trim="${r_run#"${r_run%%[!0]*}"}"
      [[ -z "$l_trim" ]] && l_trim="0"
      [[ -z "$r_trim" ]] && r_trim="0"
      if [[ ${#l_trim} -lt ${#r_trim} ]]; then
        echo "lt"
        return
      fi
      if [[ ${#l_trim} -gt ${#r_trim} ]]; then
        echo "gt"
        return
      fi
      # `<` / `>` inside `[[ ]]` is lexicographic; on equal-length pure
      # digit strings that matches numeric ordering exactly. Use it
      # instead of `(( ))` so the comparison stays correct for numbers
      # that exceed bash's integer width. shellcheck SC2071 misreads
      # the intent so disable it inline rather than reshape the code.
      # shellcheck disable=SC2071
      if [[ "$l_trim" < "$r_trim" ]]; then
        echo "lt"
        return
      fi
      # shellcheck disable=SC2071
      if [[ "$l_trim" > "$r_trim" ]]; then
        echo "gt"
        return
      fi
    else
      # shellcheck disable=SC2071
      if [[ "$l_run" < "$r_run" ]]; then
        echo "lt"
        return
      fi
      # shellcheck disable=SC2071
      if [[ "$l_run" > "$r_run" ]]; then
        echo "gt"
        return
      fi
    fi
  done
  echo "eq"
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
  local repo="$1" effective tag

  # The default shdeps repo is public, so bootstrap should not burn scarce
  # unauthenticated GitHub API quota just to learn the current tag. The normal
  # release page redirects to `/releases/tag/<tag>` and works for curl-pipe
  # installs without requiring `gh`, JSON parsing, or a token.
  effective=$(curl -fsSL -A shdeps -o /dev/null -w '%{url_effective}' \
    "https://github.com/$repo/releases/latest") || return 1
  case "$effective" in
    */releases/tag/*) ;;
    *) return 1 ;;
  esac
  tag="${effective##*/releases/tag/}"
  tag="${tag%%[?#]*}"
  [[ -n "$tag" ]] || return 1
  printf '%s\n' "$tag"
}

_verify_checksum() {
  local dir="$1" archive="$2" checksum="$3"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dir" && sha256sum -c "$checksum")
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$dir" && shasum -a 256 -c "$checksum")
  else
    _error "sha256sum or shasum is required to verify release archives"
    return 1
  fi
}

# Returns 0 (safe) only when every entry in a tar.gz archive uses a
# relative path with no `..` components. The list step uses `tar -tzf`,
# which is supported by both GNU tar and the BSD tar shipped on macOS,
# so the bootstrap script does not depend on a platform-specific
# `--no-absolute-filenames` flag. Mirrors the structural defense the
# Rust extractor (`archive.rs`) applies — keeping the two
# implementations consistent prevents the curl-pipe path from being a
# softer target than the steady-state Rust path.
_archive_entries_safe() {
  local archive="$1" entry
  if ! command -v tar >/dev/null 2>&1; then
    return 1
  fi
  # Use a process substitution-fed loop so we surface non-zero exit
  # from `tar -tzf` itself (corrupt archive, IO error) as an
  # unsafe verdict.
  while IFS= read -r entry; do
    case "$entry" in
      # Absolute paths escape the bundle root. `*../*` covers `..` as
      # any path component (`a/../b`, `./../b`, etc.). `*/..` and `..`
      # catch the trailing/standalone cases. Together they reject every
      # shell-glob representation of a traversal segment without needing
      # a full path canonicalizer.
      /*) return 1 ;;
      *../*) return 1 ;;
      */..) return 1 ;;
      ..) return 1 ;;
    esac
  done < <(tar -tzf "$archive" 2>/dev/null) || return 1
  return 0
}

_install_release_fail() {
  local tmp="$1" kind="$2" message="$3"

  # Release downloads happen before any live install is touched. Clean the
  # scratch tree on every pre-activation failure so repeated curl-pipe attempts
  # do not accumulate stale archives or make a later diagnostic ambiguous.
  _SHDEPS_RELEASE_FAILURE_KIND="$kind"
  if [[ -n "$tmp" ]]; then
    rm -rf "$tmp"
  fi
  _error "$message"
  return 1
}

_install_bundle() {
  local src_dir="$1" parent staging backup=""

  if [[ -e "$SHDEPS_DIR" ]]; then
    if [[ -d "$SHDEPS_DIR/.git" ]]; then
      if ! _release_can_replace_source_checkout "$SHDEPS_DIR"; then
        return 1
      fi
      backup="${SHDEPS_DIR}.shdeps-backup.$$"
    elif ! _is_release_install_dir "$SHDEPS_DIR"; then
      _error "$SHDEPS_DIR exists but is not a shdeps release install"
      return 1
    else
      backup="${SHDEPS_DIR}.shdeps-backup.$$"
    fi
  fi

  parent=$(dirname "$SHDEPS_DIR")
  mkdir -p "$parent"
  staging=$(mktemp -d "$parent/.shdeps-install.XXXXXX")

  # Release archives are already verified before users run their bundled
  # installer, but filesystem activation can still fail. Copy into a sibling
  # staging directory first so an interrupted or full-disk install does not
  # leave SHDEPS_DIR looking usable while missing the wrapper or metadata.
  #
  # Keep the copy in a subshell so required-file failures are contained and can
  # be cleaned up before activation. Do not rely on `set -e` here: Bash disables
  # errexit in several conditional contexts, and this subshell is intentionally
  # tested by `if ! (...)`. Explicit exits keep Bash 3.2-era installers honest.
  if ! (
    cp -p "$src_dir/shdeps" "$staging/shdeps" || exit 1
    cp -p "$src_dir/shdeps.sh" "$staging/shdeps.sh" || exit 1
    cp -p "$src_dir/install.sh" "$staging/install.sh" || exit 1
    cp -p "$src_dir/.shdeps-install.json" "$staging/.shdeps-install.json" || exit 1
    if [[ -f "$src_dir/README.md" ]]; then
      cp -p "$src_dir/README.md" "$staging/README.md" || exit 1
    fi
    if [[ -f "$src_dir/LICENSE" ]]; then
      cp -p "$src_dir/LICENSE" "$staging/LICENSE" || exit 1
    fi
    if [[ -d "$src_dir/man" ]]; then
      cp -R "$src_dir/man" "$staging/" || exit 1
    fi
    if [[ -d "$src_dir/completions" ]]; then
      cp -R "$src_dir/completions" "$staging/" || exit 1
    fi
  ); then
    rm -rf "$staging"
    return 1
  fi
  if [[ -n "$backup" ]]; then
    # Existing release installs are owned by shdeps, but still treat
    # replacement as a transaction. Move the old tree aside only after the new
    # payload is complete, then restore it if the final activation rename
    # fails. Git checkouts and unknown/manual dirs are rejected above because
    # automatic release conversion needs a separate migration path.
    #
    # Trap INT/TERM/HUP around the rename window so a Ctrl-C in the
    # exact moment between `mv SHDEPS_DIR backup` and `mv staging
    # SHDEPS_DIR` does not strand the user with `SHDEPS_DIR` missing
    # and a backup directory the next bootstrap does not understand.
    # The handler restores the backup (best-effort), cleans the
    # staging tree, and re-raises the signal so the shell exits with
    # the conventional 128+signal code instead of silently swallowing
    # the interruption.
    # Double-quoted trap body is intentional here: we want `$backup`
    # and `$staging` baked in at trap-set time so the handler is robust
    # against later reassignment. shellcheck SC2064 flags this as a
    # maintenance concern; the trade-off is acceptable for this single
    # signal-handling site.
    # shellcheck disable=SC2064
    trap "
      mv \"$backup\" \"$SHDEPS_DIR\" 2>/dev/null || true
      rm -rf \"$staging\"
      trap - INT TERM HUP
      kill -INT \$\$ 2>/dev/null || exit 130
    " INT TERM HUP
    if ! mv "$SHDEPS_DIR" "$backup"; then
      trap - INT TERM HUP
      rm -rf "$staging"
      return 1
    fi
  fi
  if ! mv "$staging" "$SHDEPS_DIR"; then
    if [[ -n "$backup" ]]; then
      mv "$backup" "$SHDEPS_DIR" 2>/dev/null || true
      trap - INT TERM HUP
    fi
    rm -rf "$staging"
    return 1
  fi
  if [[ -n "$backup" ]]; then
    # New install activated. Disarm the rollback trap before cleaning
    # the backup so a Ctrl-C during backup removal does not try to
    # restore from a directory we are deliberately deleting.
    trap - INT TERM HUP
    rm -rf "$backup"
  fi
  _info "shdeps: installed"
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

_refresh_release_install_if_stale() {
  local current latest repo

  _uses_default_repo_slug || return 0
  _is_release_install_dir "$SHDEPS_DIR" || return 0

  repo=$(_repo_slug)
  current=$(_installed_release_tag "$SHDEPS_DIR")
  # When the local install metadata is missing a `tag` field, we have no
  # comparison baseline. Treating "no current tag" as "always stale" used
  # to fire a `_install_release` call on every shell startup, hammering
  # GitHub for an install that was probably fine. A missing tag indicates
  # either a fresh install (which `_install_release` ran moments ago) or
  # a corrupted state file (which the user repairs by re-running the
  # installer explicitly). Either way, the background refresh should be
  # a no-op rather than a silent re-download loop.
  [[ -n "$current" ]] || return 0
  latest=$(_latest_release_tag "$repo") || return 0

  # Bootstrap self-update has to be conservative but useful. Release tags are
  # timestamp-prefixed (`YYYYMMDD-HHMMSS-<hash>`), so lexical ordering is enough
  # to avoid replacing a newer local archive with an older or deleted GitHub
  # "latest" release while still advancing normal fleet installs promptly.
  if _release_tag_newer "$latest" "$current"; then
    _install_release >/dev/null 2>&1 || true
  fi
}

_install_release() {
  local platform repo api_url token tmp json tag archive checksum
  local archive_url checksum_url archive_api_url checksum_api_url bundle
  _SHDEPS_RELEASE_FAILURE_KIND=""
  platform=$(_release_platform) || return 1
  repo=$(_repo_slug)
  token=$(_github_token)
  tmp=$(mktemp -d) || {
    _error "failed to create release staging directory"
    return 1
  }
  json="$tmp/release.json"

  if [[ -n "${SHDEPS_RELEASE_API_URL:-}" ]]; then
    api_url="$SHDEPS_RELEASE_API_URL"
    if ! _curl_get "$api_url" "$json" "$token"; then
      _install_release_fail "$tmp" "download" "failed to fetch shdeps release metadata"
      return 1
    fi

    tag=$(_json_string "$json" "tag_name")
    if [[ -z "$tag" ]]; then
      _install_release_fail "$tmp" "metadata" "release metadata did not contain tag_name"
      return 1
    fi

    archive="shdeps-${tag}-${platform}.tar.gz"
    checksum="${archive}.sha256"
    archive_url=$(_asset_url "$json" "$archive")
    checksum_url=$(_asset_url "$json" "$checksum")
    archive_api_url=$(_asset_api_url "$json" "$archive")
    checksum_api_url=$(_asset_api_url "$json" "$checksum")
    if [[ -z "$archive_url" || -z "$checksum_url" ]]; then
      _install_release_fail "$tmp" "metadata" "release $tag does not contain assets for $platform"
      return 1
    fi
  else
    # For the public default repo, resolve the latest tag through the normal
    # GitHub release redirect and construct canonical asset URLs. This avoids
    # unauthenticated API rate limits during fleet bootstrap while keeping the
    # archive/checksum contract explicit and easy to inspect.
    tag=$(_latest_release_tag "$repo") || {
      _install_release_fail "$tmp" "download" "failed to resolve latest shdeps release"
      return 1
    }
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
    _install_release_fail "$tmp" "download" "failed to download $archive"
    return 1
  fi
  if ! _curl_get_release_asset "$checksum_url" "$tmp/$checksum" "$token" "$checksum_api_url"; then
    _install_release_fail "$tmp" "download" "failed to download $checksum"
    return 1
  fi
  if ! _verify_checksum "$tmp" "$archive" "$checksum" >/dev/null; then
    _install_release_fail "$tmp" "artifact" "checksum verification failed for $archive"
    return 1
  fi

  bundle="$tmp/bundle"
  if ! mkdir -p "$bundle"; then
    _install_release_fail "$tmp" "artifact" "failed to create release bundle directory"
    return 1
  fi
  # Tar traversal hardening: list the archive contents before extracting
  # and refuse any entry whose path is absolute (`/foo`) or escapes the
  # destination via `..`. GNU tar's `--no-absolute-filenames` is one
  # mitigation but not portable to the BSD tar shipped on macOS; the
  # list-and-validate approach works on both. The Rust extraction path
  # (`archive.rs`) does the same check at a higher level; this is the
  # bootstrap-side equivalent for the curl-pipe install path.
  if ! _archive_entries_safe "$tmp/$archive"; then
    _install_release_fail "$tmp" "artifact" "refusing to extract $archive: contains absolute or traversal paths"
    return 1
  fi
  if ! tar -xzf "$tmp/$archive" -C "$bundle"; then
    _install_release_fail "$tmp" "artifact" "failed to extract $archive"
    return 1
  fi
  if ! _install_bundle "$bundle"; then
    _install_release_fail "$tmp" "artifact" "failed to install $archive"
    return 1
  fi
  rm -rf "$tmp"
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

  if declare -f _shdeps_self_update &>/dev/null; then
    _shdeps_self_update "$shdeps_dir"
  elif declare -f shdeps_self_update &>/dev/null; then
    # The Rust public wrapper intentionally has a clean no-arg CLI surface, so
    # pass the bootstrap-selected checkout through the environment instead of
    # preserving the legacy private helper's positional argument shape.
    local SHDEPS_DIR="$shdeps_dir"
    export SHDEPS_DIR
    shdeps_self_update
  fi
}

# Symlink CLI into PATH and link man page + shell completions.
#
# Modern installs source the Rust compatibility wrapper, but old checkouts and
# rollback fixtures may still expose the legacy Bash helper names. Keep this
# helper bilingual so installer/bootstrap activation is tolerant during fleet
# migration without spreading the legacy/private helper split to callers.
_setup_links() {
  local shdeps_dir="$1"
  local cli="$shdeps_dir/bin/shdeps-legacy"

  if [[ -x "$shdeps_dir/shdeps" ]]; then
    cli="$shdeps_dir/shdeps"
  elif [[ -x "$shdeps_dir/bin/shdeps" ]]; then
    # Current source checkouts spell the preserved Bash entry point
    # `shdeps-legacy` so repo-local direnv paths cannot shadow the Rust CLI.
    # The historical name remains a fallback only for bootstrapping older
    # installed checkouts while they are being replaced.
    cli="$shdeps_dir/bin/shdeps"
  fi

  if [[ -x "$cli" ]]; then
    mkdir -p "$(dirname "$SHDEPS_BIN")"
    ln -sf "$cli" "$SHDEPS_BIN"
  fi

  if declare -f _shdeps_link_extras &>/dev/null; then
    _shdeps_link_extras "shdeps" "$shdeps_dir"
  elif declare -f shdeps_link_extras &>/dev/null; then
    shdeps_link_extras "shdeps" "$shdeps_dir"
  fi
}

_source_installed_library_for_extras() {
  local shdeps_dir="$1"

  if [[ ! -f "$shdeps_dir/shdeps.sh" ]]; then
    return 0
  fi

  # `install.sh` is the bootstrap script users run before shdeps is installed,
  # so it must stay usable with stock macOS Bash 3.2. The sourceable legacy
  # library still needs Bash 4.3+ until the Rust wrapper cutover. Release
  # installs can still activate the Rust binary and CLI symlink under Bash 3.2;
  # they only skip optional extras linking that depends on sourcing shdeps.sh.
  if ! _bash_supports_legacy_library; then
    return 0
  fi

  # shellcheck source=/dev/null
  . "$shdeps_dir/shdeps.sh"
}

_activate_installed_tree() {
  local shdeps_dir="$1"

  _source_installed_library_for_extras "$shdeps_dir"
  _setup_links "$shdeps_dir"
}

# ---------------------------------------------------------------------------
# Install / update
# ---------------------------------------------------------------------------

_install() {
  local script_dir
  script_dir=$(_script_dir) || exit 1

  if _is_bundle_dir "$script_dir"; then
    _install_bundle "$script_dir" || exit 1
    _activate_installed_tree "$SHDEPS_DIR"
    return
  fi

  if _uses_default_repo_slug && ! _is_source_checkout_dir "$script_dir"; then
    _install_release || exit 1
    _activate_installed_tree "$SHDEPS_DIR"
    return
  fi

  _check_source_prereqs

  if [[ -d "$SHDEPS_DIR/.git" ]]; then
    # Already installed — pull latest if clean
    if [[ -n "$(git -C "$SHDEPS_DIR" status --porcelain --untracked-files=normal 2>/dev/null)" ]]; then
      _info "shdeps: dirty working tree, skipping update"
    elif git -C "$SHDEPS_DIR" pull --ff-only --quiet 2>&1; then
      _info "shdeps: updated"
    else
      _error "shdeps: update failed (git pull --ff-only failed)"
      exit 1
    fi
  elif _is_release_install_dir "$SHDEPS_DIR"; then
    # Direct source installs are explicit developer/source mode. If a managed
    # release payload is present, clean it before cloning the source install so
    # the selected implementation has a single owner on disk.
    if ! rm -rf "$SHDEPS_DIR"; then
      _error "failed to remove stale shdeps release install at $SHDEPS_DIR"
      exit 1
    fi
    _info "shdeps: cloning to $SHDEPS_DIR..."
    git clone --depth 1 "$SHDEPS_REPO" "$SHDEPS_DIR" || exit 1
    _info "shdeps: installed"
  elif [[ -d "$SHDEPS_DIR" ]]; then
    _error "$SHDEPS_DIR exists but is not a git repo"
    exit 1
  else
    _info "shdeps: cloning to $SHDEPS_DIR..."
    git clone --depth 1 "$SHDEPS_REPO" "$SHDEPS_DIR"
    _info "shdeps: installed"
  fi

  _ensure_source_checkout_binary "$SHDEPS_DIR" || exit 1

  # Source the library and set up all symlinks (CLI, man, completions).
  _activate_installed_tree "$SHDEPS_DIR"

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
# Finds shdeps.sh, sources it, symlinks the CLI, and runs self-update.
# Clients set env vars (SHDEPS_CONF_DIR, SHDEPS_HOOKS_DIR, etc.) before
# sourcing. Pre-defined _shdeps_log* functions are respected by shdeps.sh.
#
# Returns 0 if shdeps is ready, 1 if bootstrap failed.

_bootstrap() {
  # Idempotent — skip if already bootstrapped
  declare -f shdeps_update &>/dev/null && return 0

  local _bs_lib="" _bs_dir=""
  local _dev_dir="${SHDEPS_GIT_DEV_DIR:-$HOME/git}"

  # Find shdeps.sh: installed-tree env hint → env override → dev clone →
  # installed tree → fresh install. SHDEPS_LIB usually means "do exactly this",
  # but when it points back at SHDEPS_DIR it is just an older caller's cached
  # discovery result. Route that shape through the installed-tree path so it can
  # be migrated from source checkout to release assets.
  if _bootstrap_lib_is_installed_tree; then
    if _uses_default_repo_slug && [[ -d "$SHDEPS_DIR/.git" ]]; then
      _install_release >/dev/null 2>&1 || true
    else
      _refresh_release_install_if_stale
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
      _install_release >/dev/null 2>&1 || true
    else
      _refresh_release_install_if_stale
    fi
    _bs_lib="$SHDEPS_DIR/shdeps.sh"
    _bs_dir="$SHDEPS_DIR"
  else
    # Not installed — run _install in a subshell so exit doesn't kill caller
    # shellcheck disable=SC2310  # intentional: subshell contains exit
    if (_install) >/dev/null 2>&1; then
      _bs_lib="$SHDEPS_DIR/shdeps.sh"
      _bs_dir="$SHDEPS_DIR"
    else
      return 1
    fi
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

  # Pull latest shdeps (skips dirty clones / active development)
  if [[ -n "$_bs_dir" ]]; then
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
  local removed=0

  # Clean up extras symlinks (man page, completions) before removing the repo
  if [[ -f "$SHDEPS_DIR/shdeps.sh" ]]; then
    # shellcheck source=/dev/null
    . "$SHDEPS_DIR/shdeps.sh"
    if declare -f _shdeps_unlink_extras &>/dev/null; then
      _shdeps_unlink_extras "shdeps"
    elif declare -f shdeps_unlink_extras &>/dev/null; then
      shdeps_unlink_extras "shdeps"
    fi
  fi

  if [[ -L "$SHDEPS_BIN" ]]; then
    rm "$SHDEPS_BIN"
    ((removed++)) || true
  fi
  if [[ -d "$SHDEPS_DIR" ]]; then
    rm -rf "$SHDEPS_DIR"
    ((removed++)) || true
  fi
  if [[ $removed -gt 0 ]]; then
    _info "shdeps: uninstalled"
  else
    _info "shdeps: nothing to uninstall"
  fi
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
