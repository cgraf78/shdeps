#!/usr/bin/env bash
set -euo pipefail

# shdeps' runtime version is always the build commit, but release assets still
# need a stable distribution tag in their file names. Keep that concern in this
# tiny helper so packaging, smoke tests, and GitHub Actions agree without
# letting Cargo's placeholder package version leak into user-visible behavior.
tag=${SHDEPS_RELEASE_TAG:-${GITHUB_REF_NAME:-}}

if [[ -z "$tag" ]]; then
  tag=$(git describe --tags --exact-match 2>/dev/null || true)
fi

if [[ -z "$tag" ]]; then
  commit=$(git rev-parse --short HEAD)
  tag="dev-${commit}"
fi

case "$tag" in
  v*.*.* | dev-*) ;;
  *)
    printf 'release tag must look like v*.*.* or dev-<commit> (got %s)\n' "$tag" >&2
    exit 1
    ;;
esac

case "$tag" in
  *[!A-Za-z0-9._-]*)
    printf 'release tag contains characters unsafe for asset names: %s\n' "$tag" >&2
    exit 1
    ;;
esac

printf '%s\n' "$tag"
