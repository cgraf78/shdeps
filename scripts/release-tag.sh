#!/usr/bin/env bash
set -euo pipefail

# shdeps' runtime version is always the build commit, but release assets still
# need a stable distribution tag in their file names. Keep that concern in this
# tiny helper so packaging, smoke tests, and GitHub Actions agree without
# letting Cargo's placeholder package version leak into user-visible behavior.
#
# GitHub sets GITHUB_REF_NAME for branches as well as tags. Branch names are not
# release identities, and accepting them would make CI dry-runs fail on feature
# branches such as rust-port. Trust the GitHub ref only when Actions says it is a
# tag; branch and PR builds deliberately fall through to dev-<commit>.
tag=${SHDEPS_RELEASE_TAG:-}
if [[ -z "$tag" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
  tag=${GITHUB_REF_NAME:-}
fi

if [[ -z "$tag" && "${GITHUB_REF:-}" == refs/tags/* ]]; then
  tag=${GITHUB_REF#refs/tags/}
fi

if [[ -z "$tag" ]]; then
  tag=$(git describe --tags --exact-match 2>/dev/null || true)
fi

if [[ -z "$tag" && -n "${SHDEPS_BUILD_COMMIT:-}" ]]; then
  # Containerized CI can run a checkout owned by a different uid than the shell
  # user, which makes Git refuse even read-only metadata queries unless the
  # caller also wires safe.directory into that exact HOME. Release workflows
  # already know the concrete build commit, so accept that hash directly for dev
  # smoke archives and keep Git as the fallback for local convenience.
  if [[ "$SHDEPS_BUILD_COMMIT" =~ ^[0-9a-fA-F]{7,}$ ]]; then
    tag="dev-${SHDEPS_BUILD_COMMIT:0:7}"
  fi
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
