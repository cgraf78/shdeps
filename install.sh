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
#   SHDEPS_REPO         Git repo URL           (default: https://github.com/cgraf78/shdeps.git)
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

  # Source-checkout installs now build and activate the Rust binary first, so
  # stock macOS Bash 3.2 is a valid installer shell. Optional extras linking may
  # still source legacy helpers when a checkout exposes them, but that happens
  # after the CLI is installed and can degrade gracefully instead of blocking
  # fleet bootstrap on older system Bash versions.
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
  [[ -d "$dir/.git" ]]
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
  if [[ -n "$token" ]]; then
    curl -fsSL -H "Authorization: Bearer $token" -o "$out" "$url"
  else
    curl -fsSL -o "$out" "$url"
  fi
}

_repo_slug() {
  _repo_url_slug "$SHDEPS_REPO"
}

_repo_url_slug() {
  local repo="$1"

  # The same GitHub repository can be spelled several ways in bootstrap
  # contexts: public HTTPS, normal SSH, or a GitHub SSH host alias such as
  # git@github.com-shdeps:cgraf78/shdeps.git from CI deploy-key config. Compare
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

_github_ssh_fallback_url() {
  local repo="$1" path owner name
  case "$repo" in
    https://github.com/*) path="${repo#https://github.com/}" ;;
    *) return 1 ;;
  esac
  path="${path%.git}"
  owner="${path%%/*}"
  name="${path#*/}"

  # This URL is fed directly to git. Keep the fallback deliberately narrower
  # than GitHub itself so a malformed SHDEPS_REPO cannot smuggle shell-ish or
  # path-traversal text into the clone target. Users with unusual remotes can
  # still set SHDEPS_REPO explicitly and use the source-checkout path.
  case "$owner" in
    "" | *..* | *" "* | *":"* | */*) return 1 ;;
  esac
  case "$name" in
    "" | *..* | *" "* | *":"* | */*) return 1 ;;
  esac

  printf 'git@github.com:%s/%s.git\n' "$owner" "$name"
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

_asset_url() {
  local file="$1" name="$2"
  awk -v wanted="$name" '
    $0 ~ "\"name\"[[:space:]]*:[[:space:]]*\"" wanted "\"" { in_asset = 1 }
    in_asset && /"browser_download_url"[[:space:]]*:/ {
      sub(/^.*"browser_download_url"[[:space:]]*:[[:space:]]*"/, "")
      sub(/".*$/, "")
      print
      exit
    }
    $0 ~ /^[[:space:]]*\}/ { in_asset = 0 }
  ' "$file"
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
    if ! mv "$SHDEPS_DIR" "$backup"; then
      rm -rf "$staging"
      return 1
    fi
  fi
  if ! mv "$staging" "$SHDEPS_DIR"; then
    if [[ -n "$backup" ]]; then
      mv "$backup" "$SHDEPS_DIR" 2>/dev/null || true
    fi
    rm -rf "$staging"
    return 1
  fi
  if [[ -n "$backup" ]]; then
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

_cleanup_release_install_for_source_checkout() {
  local source_dir="$1" source_real install_real

  [[ -d "$source_dir/.git" ]] || return 0
  _is_release_install_dir "$SHDEPS_DIR" || return 0

  source_real=$(cd -P -- "$source_dir" 2>/dev/null && pwd) || return 0
  install_real=$(cd -P -- "$SHDEPS_DIR" 2>/dev/null && pwd) || return 0
  [[ "$source_real" != "$install_real" ]] || return 0

  # Once a developer checkout is the active implementation, a release payload
  # under SHDEPS_DIR is stale managed state. Remove only installs carrying
  # release metadata; arbitrary directories and source checkouts are handled by
  # stricter migration paths above.
  if ! rm -rf "$SHDEPS_DIR"; then
    _error "failed to remove stale shdeps release install at $SHDEPS_DIR"
    return 1
  fi
}

_install_release() {
  local platform repo api_url token tmp json tag archive checksum archive_url checksum_url bundle
  _SHDEPS_RELEASE_FAILURE_KIND=""
  platform=$(_release_platform) || return 1
  repo=$(_repo_slug)
  api_url="${SHDEPS_RELEASE_API_URL:-https://api.github.com/repos/$repo/releases/latest}"
  token=$(_github_token)
  tmp=$(mktemp -d) || {
    _error "failed to create release staging directory"
    return 1
  }
  json="$tmp/release.json"

  # curl-pipe installs have no trusted local checkout. Use the GitHub release
  # contract instead of cloning source so fresh machines do not need a Rust
  # toolchain, and so WSL/Linux avoid host glibc drift by consuming musl assets.
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
  if [[ -z "$archive_url" || -z "$checksum_url" ]]; then
    _install_release_fail "$tmp" "metadata" "release $tag does not contain assets for $platform"
    return 1
  fi

  if ! _curl_get "$archive_url" "$tmp/$archive" "$token"; then
    _install_release_fail "$tmp" "download" "failed to download $archive"
    return 1
  fi
  if ! _curl_get "$checksum_url" "$tmp/$checksum" "$token"; then
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

_install_source_build_fallback() {
  local fallback parent staging commit version

  if [[ "${_SHDEPS_RELEASE_FAILURE_KIND:-}" != "download" ]]; then
    return 1
  fi
  if [[ -e "$SHDEPS_DIR" ]]; then
    return 1
  fi
  if ! fallback=$(_github_ssh_fallback_url "$SHDEPS_REPO"); then
    return 1
  fi
  if ! command -v git >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then
    return 1
  fi

  parent=$(dirname "$SHDEPS_DIR")
  mkdir -p "$parent"
  staging=$(mktemp -d "$parent/.shdeps-source-build.XXXXXX") || return 1

  # Private source fallback exists for machines that can clone over SSH but
  # cannot read private GitHub release assets. Build in a sibling staging tree
  # and only publish it at SHDEPS_DIR after the binary exists, preserving the
  # same no-partial-live-install guarantee as release archives.
  if ! git clone --depth 1 "$fallback" "$staging"; then
    rm -rf "$staging"
    return 1
  fi
  if ! (cd "$staging" && cargo build --release --locked); then
    rm -rf "$staging"
    return 1
  fi
  if [[ ! -x "$staging/target/release/shdeps" ]]; then
    rm -rf "$staging"
    return 1
  fi
  ln -sf "target/release/shdeps" "$staging/shdeps"
  commit=$(git -C "$staging" rev-parse HEAD 2>/dev/null || true)
  version=$("$staging/target/release/shdeps" version 2>/dev/null | sed -n 's/^shdeps //p' | head -n 1 || true)
  cat >"$staging/.shdeps-install.json" <<JSON
{"schema":1,"method":"source-build","repo":"$(_repo_slug)","version":"$version","commit":"$commit"}
JSON
  if ! mv "$staging" "$SHDEPS_DIR"; then
    rm -rf "$staging"
    return 1
  fi
  _info "shdeps: installed from source"
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
    if ! _install_release; then
      _install_source_build_fallback || exit 1
    fi
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
    _bootstrap_self_update "$_bs_dir" 2>/dev/null || true
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
  [[ -n "$_bs_dir" ]] && _cleanup_release_install_for_source_checkout "$_bs_dir"
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
