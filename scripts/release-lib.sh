# shellcheck shell=bash
#
# Shared release logic for cgraf78 Rust repositories.
#
# Sourced, never executed: no shebang and no executable bit.
#
# This library is the single source of truth for release identity, packaging,
# and smoke validation. Consumer repositories vendor it verbatim into
# `scripts/release-lib.sh` and describe only what genuinely differs between
# projects in `scripts/release.conf`. CI verifies the vendored copy still
# matches this file, so the copies cannot drift apart silently.
#
# Everything here is sourced by thin entry-point scripts that own `set -euo
# pipefail`. Functions therefore return non-zero instead of calling `exit`, and
# do not mutate the caller's shell options.

# Resolve the consumer repository root from this library's own location rather
# than the caller's working directory. Release commands run from CI job steps,
# `cargo` wrappers, and interactive shells, and the crate layout is the fixed
# reference point in all three.
RELEASE_SCRIPTS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
RELEASE_REPO_ROOT=$(cd "$RELEASE_SCRIPTS_DIR/.." && pwd)

# The config contract, in one place. release_load_config fills these in from
# scripts/release.conf and rejects the ones that are still empty but required.
# Declaring them here keeps the library's inputs discoverable without opening a
# consumer's config, and lets static analysis see them.
RELEASE_ENV_PREFIX=${RELEASE_ENV_PREFIX:-}
RELEASE_SLUG=${RELEASE_SLUG:-}
RELEASE_ASSET_NAME=${RELEASE_ASSET_NAME:-}
RELEASE_BINARY=${RELEASE_BINARY:-}
RELEASE_BINARY_DEST=${RELEASE_BINARY_DEST:-}
RELEASE_REPO=${RELEASE_REPO:-}
RELEASE_PAYLOAD_FILES=(${RELEASE_PAYLOAD_FILES[@]+"${RELEASE_PAYLOAD_FILES[@]}"})
RELEASE_PAYLOAD_DIRS=(${RELEASE_PAYLOAD_DIRS[@]+"${RELEASE_PAYLOAD_DIRS[@]}"})

# Derived from RELEASE_SLUG by release_load_config.
RELEASE_VERSION_FILE=""
RELEASE_INSTALL_METADATA_FILE=""

release_die() {
  printf 'release: %s\n' "$*" >&2
  return 1
}

# Build the fully qualified name of a per-project release environment variable,
# e.g. release_env_name BUILD_COMMIT -> HIVE_MEMORY_BUILD_COMMIT.
release_env_name() {
  printf '%s_%s' "$RELEASE_ENV_PREFIX" "$1"
}

# Read a per-project release environment variable, empty when unset.
release_env_get() {
  local name
  name=$(release_env_name "$1")
  printf '%s' "${!name:-}"
}

# Load and validate scripts/release.conf, then derive the values every consumer
# would otherwise repeat. Must be called before any other function here.
release_load_config() {
  local config="$RELEASE_SCRIPTS_DIR/release.conf"

  [[ -f "$config" ]] || {
    release_die "missing release config: $config"
    return 1
  }

  # shellcheck source=/dev/null
  . "$config"

  local required
  for required in RELEASE_ENV_PREFIX RELEASE_SLUG RELEASE_ASSET_NAME RELEASE_BINARY; do
    [[ -n "${!required:-}" ]] || {
      release_die "$config must set $required"
      return 1
    }
  done

  # The env prefix is spliced into variable names via indirect expansion, so
  # restrict it to characters that can legally form a shell identifier.
  [[ "$RELEASE_ENV_PREFIX" =~ ^[A-Z][A-Z0-9_]*$ ]] || {
    release_die "RELEASE_ENV_PREFIX must be an uppercase shell identifier: $RELEASE_ENV_PREFIX"
    return 1
  }

  # Archive and metadata filenames are built from these, so reject anything
  # that would produce an unsafe or ambiguous asset name.
  local safe
  for safe in RELEASE_SLUG RELEASE_ASSET_NAME; do
    case "${!safe}" in
      *[!A-Za-z0-9._-]*)
        release_die "$safe contains characters unsafe for asset names: ${!safe}"
        return 1
        ;;
    esac
  done

  : "${RELEASE_REPO:=cgraf78/$RELEASE_SLUG}"
  # Most projects ship the binary at the archive root; those that use a bin/
  # layout override this.
  : "${RELEASE_BINARY_DEST:=$RELEASE_BINARY}"

  RELEASE_VERSION_FILE=".${RELEASE_SLUG}-release-version"
  RELEASE_INSTALL_METADATA_FILE=".${RELEASE_SLUG}-install.json"
}

# --- Release identity -------------------------------------------------------
#
# The public version is generated from the UTC committer timestamp plus an
# 8-hex commit suffix. Cargo.toml is deliberately not the release-version source
# of truth: the same commit must always map to the same release version, and the
# hash suffix keeps source-only installs and published assets traceable back to
# the exact git history they came from.

release_valid_commit() {
  [[ "${1:-}" =~ ^[0-9a-fA-F]{8,}$ ]]
}

release_valid_timestamp() {
  [[ "${1:-}" =~ ^[0-9]{8}-[0-9]{6}$ ]]
}

release_valid_version() {
  [[ "${1:-}" =~ ^[0-9]{8}-[0-9]{6}-[0-9a-fA-F]{8}$ ]]
}

_release_commit_timestamp() {
  # Git's `format-local` uses the process timezone, so forcing TZ=UTC gives the
  # same YYYYMMDD-HHMMSS tag prefix on developer laptops and CI runners. This
  # keeps release tags deterministic for a commit without depending on GNU-only
  # or BSD-only `date` epoch conversion flags.
  TZ=UTC git -C "$RELEASE_REPO_ROOT" show -s \
    --date=format-local:%Y%m%d-%H%M%S --format=%cd "$1" 2>/dev/null || true
}

# Read the release version implied by a tag ref, when the build is running from
# one. Empty output means the ref is not a tag.
_release_ref_version() {
  local tag=""

  if [[ "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
    tag=${GITHUB_REF_NAME:-}
  elif [[ "${GITHUB_REF:-}" == refs/tags/* ]]; then
    tag=${GITHUB_REF#refs/tags/}
  fi

  if [[ -n "$tag" ]]; then
    release_valid_version "$tag" ||
      release_die "release tag must look like YYYYMMDD-HHMMSS-<8hex> (got $tag)" || return 1
    printf '%s' "$tag"
  fi
}

# Resolve the commit this build represents, preferring explicit build env so
# packaging jobs and source snapshots can pin it.
release_build_commit() {
  local commit
  commit=$(release_env_get BUILD_COMMIT)
  [[ -n "$commit" ]] || commit=${GITHUB_SHA:-}
  [[ -n "$commit" ]] || commit=$(git -C "$RELEASE_REPO_ROOT" rev-parse HEAD 2>/dev/null || true)

  release_valid_commit "$commit" ||
    release_die "build commit must be a concrete git hash of at least 8 hex chars" || return 1

  printf '%s' "$commit"
}

# Print the single public version string used by release tags, archive names,
# packaged metadata, and the tool's own `--version` output.
release_compute_version() {
  local commit version version_commit timestamp build_version

  commit=$(release_build_commit) || return 1

  # An explicit build version wins, but must still describe this commit so a
  # mislabelled archive cannot be produced by a stray environment variable.
  build_version=$(release_env_get BUILD_VERSION)
  if [[ -n "$build_version" ]]; then
    release_valid_version "$build_version" ||
      release_die "$(release_env_name BUILD_VERSION) must look like YYYYMMDD-HHMMSS-<8hex>" || return 1
    version_commit=${build_version##*-}
    if [[ "${commit:0:8}" != "$version_commit" ]]; then
      release_die "$(release_env_name BUILD_VERSION) commit suffix $version_commit does not match build commit ${commit:0:8}"
      return 1
    fi
    printf '%s\n' "$build_version"
    return 0
  fi

  version=$(_release_ref_version) || return 1
  if [[ -n "$version" ]]; then
    version_commit=${version##*-}
    if [[ "${commit:0:8}" != "$version_commit" ]]; then
      release_die "release tag commit suffix $version_commit does not match build commit ${commit:0:8}"
      return 1
    fi
    printf '%s\n' "$version"
    return 0
  fi

  timestamp=$(release_env_get BUILD_TIMESTAMP)
  [[ -n "$timestamp" ]] || timestamp=$(_release_commit_timestamp "$commit")
  # `date -u +FORMAT` is available on GNU and BSD/macOS date. This is the
  # fallback for source snapshots that provide a commit but no git object.
  [[ -n "$timestamp" ]] || timestamp=$(date -u +%Y%m%d-%H%M%S)

  release_valid_timestamp "$timestamp" ||
    release_die "$(release_env_name BUILD_TIMESTAMP) must look like YYYYMMDD-HHMMSS" || return 1

  printf '%s-%s\n' "$timestamp" "${commit:0:8}"
}

# Print the release version, additionally guaranteeing it is safe to splice into
# asset filenames. Packaging and smoke both key off this.
release_compute_tag() {
  local tag
  tag=$(release_compute_version) || return 1

  case "$tag" in
    *[!A-Za-z0-9._-]*)
      release_die "release tag contains characters unsafe for asset names: $tag"
      return 1
      ;;
  esac

  printf '%s\n' "$tag"
}

# --- Packaging --------------------------------------------------------------

# Reject Rust target triples where a public platform label is expected. Triples
# carry a vendor field (`unknown` for the targets we build), which is compiler
# plumbing rather than user-facing release identity.
release_validate_asset_platform() {
  case "${1:-}" in
    '' | *[!A-Za-z0-9._-]*)
      release_die "asset platform contains characters unsafe for asset names: ${1:-}"
      return 2
      ;;
    *unknown*)
      release_die "asset platform must be a public release label, not a Rust target triple: $1"
      return 2
      ;;
  esac
}

# Copy one payload entry into the staging tree, preserving the source's
# executable bit so shipped installers and wrappers keep working.
_release_stage_file() {
  local src=$1 dest=$2 mode=0644

  [[ -f "$src" ]] || {
    release_die "declared payload file is missing: $src"
    return 1
  }
  [[ -x "$src" ]] && mode=0755

  mkdir -p "$(dirname "$dest")"
  install -m "$mode" "$src" "$dest"
}

# Build the release archive and checksum for one Rust target, and print the
# archive path.
release_package() {
  local target=$1 asset_platform=$2
  local target_dir commit tag asset dist_dir

  release_validate_asset_platform "$asset_platform" || return $?

  cd "$RELEASE_REPO_ROOT" || return 1

  # Cargo's target directory is caller-configurable so CI can keep large build
  # trees out of source checkouts that are subsequently copied into constrained
  # runtimes such as Termux. Read the binary from the same location Cargo used.
  target_dir=${CARGO_TARGET_DIR:-target}

  tag=$(release_compute_tag) || return 1
  commit=$(release_build_commit) || return 1

  asset="${RELEASE_ASSET_NAME}-${tag}-${asset_platform}.tar.gz"
  dist_dir=dist
  mkdir -p "$dist_dir"

  # Stage inside a subshell so an EXIT trap owns cleanup: EXIT fires on every
  # path out, including a hard `set -e` abort. A RETURN trap would be wrong
  # here because Bash also runs RETURN traps when a sourced file finishes.
  (
    staging=$(mktemp -d)
    trap 'rm -rf "$staging"' EXIT

    # Release archives are consumed by bootstrap paths that may not have a Rust
    # toolchain or a source checkout. Build the exact target binary first, then
    # package only the executable and the declared runtime payload.
    env \
      "$(release_env_name BUILD_COMMIT)=$commit" \
      "$(release_env_name BUILD_VERSION)=$tag" \
      cargo build --release --locked --target "$target" || exit 1

    mkdir -p "$(dirname "$staging/$RELEASE_BINARY_DEST")"
    install -m 0755 "${target_dir}/${target}/release/${RELEASE_BINARY}" \
      "$staging/$RELEASE_BINARY_DEST" || exit 1

    for entry in ${RELEASE_PAYLOAD_FILES[@]+"${RELEASE_PAYLOAD_FILES[@]}"}; do
      _release_stage_file "$entry" "$staging/$entry" || exit 1
    done

    # Payload directories are optional by design: repos gain and lose generated
    # asset trees (completions, schemas, templates) over time, and a release
    # should not fail because an optional tree has not been generated yet.
    for entry in ${RELEASE_PAYLOAD_DIRS[@]+"${RELEASE_PAYLOAD_DIRS[@]}"}; do
      if [[ -d "$entry" ]]; then
        mkdir -p "$staging/$entry"
        cp -R "$entry/." "$staging/$entry/" || exit 1
      fi
    done

    # Include install metadata in the archive so an extracted release can be
    # identified without guessing from filesystem shape. Activation code may
    # rewrite or enrich this file at install time, but the packaged copy gives
    # bundled-mode installer tests a concrete release identity to validate.
    cat >"$staging/$RELEASE_INSTALL_METADATA_FILE" <<EOF
{
  "schema": 1,
  "method": "release",
  "artifact_platform": "$asset_platform",
  "version": "$tag",
  "tag": "$tag",
  "commit": "$commit",
  "repo": "$RELEASE_REPO"
}
EOF

    # Local package+smoke loops are not anchored by a pushed release tag. Record
    # the version that was actually packaged so smoke always verifies the archive
    # produced by the immediately preceding package step. Real tag releases remain
    # governed by the GitHub tag and do not depend on this ignored breadcrumb.
    printf '%s\n' "$tag" >"${dist_dir}/${RELEASE_VERSION_FILE}"
    tar -C "$staging" -czf "${dist_dir}/${asset}" . || exit 1
  ) || return 1

  # GNU coreutils and macOS expose different checksum commands. Prefer
  # `sha256sum` when present, but keep archive creation native on macOS runners
  # so releases do not depend on Homebrew bootstrap state.
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dist_dir" && sha256sum "$asset") >"${dist_dir}/${asset}.sha256"
  else
    (cd "$dist_dir" && shasum -a 256 "$asset") >"${dist_dir}/${asset}.sha256"
  fi

  printf '%s\n' "${dist_dir}/${asset}"
}

# --- Smoke validation -------------------------------------------------------

# Locate the archive for this platform, falling back to the breadcrumb written
# by the preceding package step for local dry-runs.
_release_archive_path() {
  local asset_platform=$1 tag archive

  tag=$(release_compute_tag) || return 1
  archive="dist/${RELEASE_ASSET_NAME}-${tag}-${asset_platform}.tar.gz"

  if [[ ! -f "$archive" && -f "dist/$RELEASE_VERSION_FILE" ]]; then
    tag=$(<"dist/$RELEASE_VERSION_FILE")
    case "$tag" in
      '' | *[!A-Za-z0-9._-]*)
        release_die "recorded release version is unsafe for asset names: $tag"
        return 1
        ;;
    esac
    archive="dist/${RELEASE_ASSET_NAME}-${tag}-${asset_platform}.tar.gz"
  fi

  printf '%s' "$archive"
}

# Extract and validate the packaged archive for one platform.
#
# Generic coverage is intentionally small and packaging-focused: it catches a
# missing executable bit, an omitted payload entry, or archive name drift. Repos
# that also want runtime coverage provide scripts/release-smoke-hook.sh defining
# release_smoke_check <extract-dir> <asset-platform>.
release_smoke() {
  local asset_platform=$1
  local archive hook

  release_validate_asset_platform "$asset_platform" || return $?

  cd "$RELEASE_REPO_ROOT" || return 1

  archive=$(_release_archive_path "$asset_platform") || return 1
  [[ -f "$archive" ]] || {
    release_die "release archive not found: $archive"
    return 1
  }

  hook="$RELEASE_SCRIPTS_DIR/release-smoke-hook.sh"

  # Validate inside a subshell so an EXIT trap owns cleanup. This must not be a
  # RETURN trap: Bash runs RETURN traps when a sourced file finishes, so
  # sourcing the repo hook below would delete the extracted tree before the
  # hook could read it.
  (
    smoke=$(mktemp -d)
    trap 'rm -rf "$smoke"' EXIT

    tar -xzf "$archive" -C "$smoke" || exit 1

    [[ -x "$smoke/$RELEASE_BINARY_DEST" ]] || {
      release_die "packaged binary is missing or not executable: $RELEASE_BINARY_DEST"
      exit 1
    }
    [[ -f "$smoke/$RELEASE_INSTALL_METADATA_FILE" ]] || {
      release_die "packaged install metadata is missing: $RELEASE_INSTALL_METADATA_FILE"
      exit 1
    }
    for entry in ${RELEASE_PAYLOAD_FILES[@]+"${RELEASE_PAYLOAD_FILES[@]}"}; do
      [[ -f "$smoke/$entry" ]] || {
        release_die "declared payload file missing from archive: $entry"
        exit 1
      }
      # A shipped installer or wrapper that arrives without its executable bit
      # fails much later, in the consumer's bootstrap.
      if [[ -x "$entry" && ! -x "$smoke/$entry" ]]; then
        release_die "payload file lost its executable bit in the archive: $entry"
        exit 1
      fi
    done

    # Payload directories are optional, but one that exists in the checkout and
    # is missing from the archive is a packaging failure, not a valid absence.
    for entry in ${RELEASE_PAYLOAD_DIRS[@]+"${RELEASE_PAYLOAD_DIRS[@]}"}; do
      if [[ -d "$entry" && ! -d "$smoke/$entry" ]]; then
        release_die "declared payload directory missing from archive: $entry"
        exit 1
      fi
    done

    if [[ "$asset_platform" == android-* ]]; then
      # Android artifacts are cross-built on an x86_64 runner. Validate the
      # architecture and Bionic loader without trying to execute them on glibc.
      _release_check_android "$smoke/$RELEASE_BINARY_DEST" "$asset_platform" || exit 1
      exit 0
    fi

    if [[ -f "$hook" ]]; then
      # shellcheck source=/dev/null
      . "$hook"
      # A hook that exists but defines nothing would otherwise fail with a bare
      # "command not found" that reads like a broken archive.
      declare -F release_smoke_check >/dev/null || {
        release_die "$hook must define release_smoke_check"
        exit 1
      }
      release_smoke_check "$smoke" "$asset_platform" || exit 1
    fi
  ) || return 1
}

_release_check_android() {
  local binary=$1 asset_platform=$2 machine

  case "$asset_platform" in
    android-aarch64) machine='AArch64' ;;
    android-x86_64) machine='X86-64' ;;
    *)
      release_die "unsupported Android asset platform: $asset_platform"
      return 1
      ;;
  esac

  # readelf prefixes some architectures with a vendor string, e.g.
  # "Advanced Micro Devices X86-64", so anchor on the field and match the
  # architecture anywhere after it rather than immediately after the spaces.
  readelf -h "$binary" | grep -Eq "Machine:[[:space:]].*${machine}" || {
    release_die "packaged Android binary is not $machine"
    return 1
  }
  readelf -l "$binary" | grep -Fq '/system/bin/linker64' || {
    release_die "packaged Android binary does not use the Bionic loader"
    return 1
  }
}

# --- Local release cutting --------------------------------------------------

# Drop a tag that was created but never successfully pushed, so a failed
# `--push` run leaves no local tag that would block the next attempt.
#
# Relies on Bash dynamic scoping to read release_cut's locals. It exists only as
# release_cut's RETURN trap and must not be called from anywhere else.
_release_cleanup_unpushed_tag() {
  if [[ "$push" == 1 && "$created_tag" == 1 && "$pushed_tag" == 0 ]]; then
    git tag -d "$tag" >/dev/null 2>&1 || true
  fi
}

_release_cut_usage() {
  cat >&2 <<EOF
usage: scripts/release.sh [--push] [--dry-run]

Create the UTC commit-timestamp/hash release tag for the current main commit.

Options:
  --push     push main and the generated tag to origin
  --dry-run  validate and print the tag/actions without creating anything
EOF
}

release_restore_bounded_command_traps() {
  if [[ -n "$old_hup_trap" ]]; then eval "$old_hup_trap"; else trap - HUP; fi
  if [[ -n "$old_int_trap" ]]; then eval "$old_int_trap"; else trap - INT; fi
  if [[ -n "$old_term_trap" ]]; then eval "$old_term_trap"; else trap - TERM; fi
}

release_signal_command_group() {
  local root=$1 signal=$2 member members

  members=$(ps -eo pid=,pgid= 2>/dev/null |
    awk -v group="$root" -v root="$root" '$2 == group && $1 != root { print $1 }')
  for member in $members; do
    kill -s "$signal" "$member" 2>/dev/null || true
  done
  kill -s "$signal" "$root" 2>/dev/null || true
}

release_abort_bounded_command() {
  local signal=$1

  if [[ -n "$pid" ]]; then
    release_signal_command_group "$pid" TERM
    release_signal_command_group "$pid" KILL
    wait "$pid" 2>/dev/null || true
  fi
  if [[ -n "$watchdog" ]]; then
    kill "$watchdog" 2>/dev/null || true
    wait "$watchdog" 2>/dev/null || true
  fi
  [[ "$monitor_enabled" == 1 ]] || set +m
  rm -f "$marker"
  release_restore_bounded_command_traps
  kill -s "$signal" "$bounded_command_shell_pid"
}

release_bounded_command() {
  local timeout_seconds=${RELEASE_NETWORK_TIMEOUT_SECONDS:-120}
  local kill_after=${RELEASE_NETWORK_KILL_AFTER_SECONDS:-5}
  local bounded_command_shell_pid marker old_hup_trap old_int_trap old_term_trap
  local monitor_enabled=0 pid='' watchdog='' status

  marker=$(mktemp "${TMPDIR:-/tmp}/release-network-timeout.XXXXXX") || return 1
  rm -f "$marker"
  bounded_command_shell_pid=$(sh -c 'printf "%s\n" "$PPID"')
  old_hup_trap=$(trap -p HUP)
  old_int_trap=$(trap -p INT)
  old_term_trap=$(trap -p TERM)
  trap 'release_abort_bounded_command HUP' HUP
  trap 'release_abort_bounded_command INT' INT
  trap 'release_abort_bounded_command TERM' TERM
  [[ $- != *m* ]] || monitor_enabled=1
  set -m
  "$@" &
  pid=$!
  [[ "$monitor_enabled" == 1 ]] || set +m
  (
    local sleeper=
    trap '[[ -z "$sleeper" ]] || kill "$sleeper" 2>/dev/null || true' EXIT
    trap 'exit 0' HUP INT TERM

    sleep "$timeout_seconds" &
    sleeper=$!
    wait "$sleeper" 2>/dev/null || exit 0
    sleeper=
    if kill -0 "$pid" 2>/dev/null; then
      : >"$marker"
      release_signal_command_group "$pid" TERM
      sleep "$kill_after" &
      sleeper=$!
      wait "$sleeper" 2>/dev/null || exit 0
      sleeper=
      release_signal_command_group "$pid" KILL
    fi
  ) &
  watchdog=$!
  if wait "$pid"; then status=0; else status=$?; fi
  if [[ ! -f "$marker" ]]; then
    kill "$watchdog" 2>/dev/null || true
  fi
  wait "$watchdog" 2>/dev/null || true
  release_restore_bounded_command_traps
  if [[ -f "$marker" ]]; then status=124; fi
  rm -f "$marker"
  return "$status"
}

release_network_git() {
  GIT_TERMINAL_PROMPT=0 \
    GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh -oBatchMode=yes -oConnectTimeout=15 -oServerAliveInterval=15 -oServerAliveCountMax=2}" \
    release_bounded_command git \
    -c http.lowSpeedLimit=1024 -c http.lowSpeedTime=30 "$@"
}

release_remote_ref_matches() {
  local remote=$1 ref=$2 expected=$3 output actual

  output=$(release_network_git ls-remote --exit-code "$remote" "$ref") || return 1
  actual=${output%%[[:space:]]*}
  [[ "$actual" == "$expected" ]]
}

# Cut (and optionally publish) the release tag for the current HEAD.
release_cut() {
  local push=0 dry_run=0
  local remote branch remote_ref head_commit remote_commit tag tag_type tagged_commit
  local remote_tag_status gh_error push_status tag_exists=0

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --push) push=1 ;;
      --dry-run) dry_run=1 ;;
      -h | --help)
        _release_cut_usage
        return 0
        ;;
      *)
        _release_cut_usage
        return 2
        ;;
    esac
    shift
  done

  remote=$(release_env_get RELEASE_REMOTE)
  [[ -n "$remote" ]] || remote=origin
  branch=$(release_env_get RELEASE_BRANCH)
  [[ -n "$branch" ]] || branch=main
  remote_ref="refs/remotes/$remote/$branch"

  cd "$RELEASE_REPO_ROOT" || return 1

  [[ $(git branch --show-current) == "$branch" ]] || {
    release_die "run from $branch"
    return 1
  }
  [[ -z $(git status --porcelain) ]] || {
    release_die "worktree must be clean"
    return 1
  }

  # Fetch before deriving the tag so local release cuts are anchored to the same
  # commit that GitHub will build. The explicit refspec keeps this independent of
  # whatever upstream tracking configuration a developer happens to have locally.
  release_network_git fetch --quiet --tags "$remote" \
    "+refs/heads/$branch:$remote_ref" || return 1

  head_commit=$(git rev-parse HEAD)
  remote_commit=$(git rev-parse "$remote_ref")
  if [[ "$head_commit" != "$remote_commit" ]]; then
    release_die "local $branch ($head_commit) does not match $remote/$branch ($remote_commit)"
    return 1
  fi

  # Version resolution intentionally honors several CI/build environment
  # overrides because packaging jobs and source snapshots need that flexibility.
  # The release cutter has a narrower contract: publish the clean local HEAD that
  # was just verified against origin/main. Keep ambient shell/GitHub env from
  # changing which tag this creates.
  tag=$(
    unset "$(release_env_name BUILD_VERSION)" "$(release_env_name BUILD_TIMESTAMP)" \
      GITHUB_REF_TYPE GITHUB_REF_NAME GITHUB_REF GITHUB_SHA
    export "$(release_env_name BUILD_COMMIT)=$head_commit"
    release_compute_tag
  ) || return 1

  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    tag_type=$(git cat-file -t "refs/tags/$tag")
    if [[ "$tag_type" != "commit" ]]; then
      release_die "local tag $tag is a $tag_type object; release tags must be lightweight"
      return 1
    fi
    tagged_commit=$(git rev-list -n 1 "$tag")
    if [[ "$tagged_commit" != "$head_commit" ]]; then
      release_die "local tag $tag already points at $tagged_commit, not HEAD $head_commit"
      return 1
    fi
    tag_exists=1
  fi

  if release_network_git ls-remote --exit-code --tags "$remote" \
    "refs/tags/$tag" >/dev/null 2>&1; then
    remote_tag_status=0
  else
    remote_tag_status=$?
  fi
  if [[ "$remote_tag_status" -eq 0 ]]; then
    release_die "$remote already has tag $tag"
    return 1
  elif [[ "$remote_tag_status" -ne 2 ]]; then
    release_die "cannot determine whether $remote already has tag $tag"
    return "$remote_tag_status"
  fi

  if command -v gh >/dev/null 2>&1; then
    gh_error=$(mktemp "${TMPDIR:-/tmp}/release-gh-view.XXXXXX") || return 1
    if release_bounded_command gh release view "$tag" >/dev/null 2>"$gh_error"; then
      rm -f "$gh_error"
      release_die "GitHub release $tag already exists"
      return 1
    elif ! grep -Eqi 'release not found|HTTP 404|status.?404|none of the git remotes configured|To use GitHub CLI in a GitHub Actions workflow|To get started with GitHub CLI|not logged into any GitHub hosts' \
      "$gh_error"; then
      cat "$gh_error" >&2
      rm -f "$gh_error"
      release_die "cannot determine whether GitHub release $tag exists"
      return 1
    fi
    rm -f "$gh_error"
  fi

  if [[ "$dry_run" == 1 ]]; then
    printf 'release: tag %s\n' "$tag"
    if [[ "$tag_exists" == 1 ]]; then
      printf 'release: local tag %s already exists at HEAD\n' "$tag"
    else
      printf 'release: would create local tag %s at %s\n' "$tag" "$head_commit"
    fi
    if [[ "$push" == 1 ]]; then
      printf 'release: would push %s and refs/tags/%s to %s\n' "$branch" "$tag" "$remote"
    fi
    return 0
  fi

  # Use a lightweight tag on purpose. Release identity embeds the target commit
  # suffix in the tag itself; annotated tags add a separate tag-object hash that
  # provides no value here and can confuse CI environments that expose the
  # triggering SHA differently for annotated tag pushes.
  local created_tag=1 pushed_tag=0
  trap _release_cleanup_unpushed_tag RETURN

  if [[ "$tag_exists" == 1 ]]; then
    created_tag=0
    printf 'release: local tag %s already exists at HEAD\n' "$tag"
  else
    git tag "$tag" || return 1
    printf 'release: created tag %s\n' "$tag"
  fi

  if [[ "$push" == 1 ]]; then
    if release_network_git push --quiet "$remote" "$branch"; then
      push_status=0
    else
      push_status=$?
    fi
    if [[ "$push_status" -ne 0 ]]; then
      release_remote_ref_matches "$remote" "refs/heads/$branch" "$head_commit" ||
        return "$push_status"
      printf 'release: branch push reached %s after an ambiguous client error\n' "$remote"
    fi
    if release_network_git push --quiet "$remote" "refs/tags/$tag"; then
      push_status=0
    else
      push_status=$?
    fi
    if [[ "$push_status" -ne 0 ]]; then
      release_remote_ref_matches "$remote" "refs/tags/$tag" "$head_commit" ||
        return "$push_status"
      printf 'release: tag push reached %s after an ambiguous client error\n' "$remote"
    fi
    pushed_tag=1
    printf 'release: pushed tag %s\n' "$tag"
  else
    printf 'release: tag %s is local only; rerun with --push to publish\n' "$tag"
  fi

  printf 'release: %s\n' "$tag"
}
