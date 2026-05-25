#!/usr/bin/env bash
set -euo pipefail

# Compute the single public version string used by release tags, archive names,
# packaged metadata, and `shdeps version`.
#
# shdeps no longer has a hand-maintained VERSION file, but release binaries
# still need a readable identity. The timestamp makes it obvious when the build
# identity was minted, while the commit suffix keeps source-only installs and
# release assets traceable back to the exact git history they came from.

_valid_commit() {
  [[ "${1:-}" =~ ^[0-9a-fA-F]{8,}$ ]]
}

_valid_timestamp() {
  [[ "${1:-}" =~ ^[0-9]{8}-[0-9]{6}$ ]]
}

_valid_version() {
  [[ "${1:-}" =~ ^[0-9]{8}-[0-9]{6}-[0-9a-fA-F]{8}$ ]]
}

_die() {
  printf '%s\n' "$*" >&2
  exit 1
}

_commit_from_git() {
  git rev-parse HEAD 2>/dev/null || true
}

_current_timestamp() {
  # `date -u +FORMAT` is available on GNU and BSD/macOS date. Keep this helper
  # shell-portable because release creation is allowed from developer laptops,
  # not just from a homogeneous CI image.
  date -u +%Y%m%d-%H%M%S
}

_ref_version() {
  local tag=""

  if [[ "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
    tag=${GITHUB_REF_NAME:-}
  elif [[ "${GITHUB_REF:-}" == refs/tags/* ]]; then
    tag=${GITHUB_REF#refs/tags/}
  fi

  if [[ -n "$tag" ]]; then
    _valid_version "$tag" || _die "release tag must look like YYYYMMDD-HHMMSS-<8hex> (got $tag)"
    printf '%s\n' "$tag"
  fi
}

commit=${SHDEPS_BUILD_COMMIT:-${GITHUB_SHA:-}}
if [[ -z "$commit" ]]; then
  commit=$(_commit_from_git)
fi
_valid_commit "$commit" || _die "build commit must be a concrete git hash of at least 8 hex chars"

if [[ -n "${SHDEPS_BUILD_VERSION:-}" ]]; then
  _valid_version "$SHDEPS_BUILD_VERSION" || _die "SHDEPS_BUILD_VERSION must look like YYYYMMDD-HHMMSS-<8hex>"
  version_commit=${SHDEPS_BUILD_VERSION##*-}
  if [[ "${commit:0:8}" != "$version_commit" ]]; then
    _die "SHDEPS_BUILD_VERSION commit suffix $version_commit does not match build commit ${commit:0:8}"
  fi
  printf '%s\n' "$SHDEPS_BUILD_VERSION"
  exit 0
fi

version=$(_ref_version)
if [[ -n "$version" ]]; then
  version_commit=${version##*-}
  if [[ "${commit:0:8}" != "$version_commit" ]]; then
    _die "release tag commit suffix $version_commit does not match build commit ${commit:0:8}"
  fi
  printf '%s\n' "$version"
  exit 0
fi

timestamp=${SHDEPS_BUILD_TIMESTAMP:-}
if [[ -z "$timestamp" ]]; then
  timestamp=$(_current_timestamp)
fi
_valid_timestamp "$timestamp" || _die "SHDEPS_BUILD_TIMESTAMP must look like YYYYMMDD-HHMMSS"

printf '%s-%s\n' "$timestamp" "${commit:0:8}"
