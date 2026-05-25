#!/usr/bin/env bash
set -euo pipefail

tag=$(scripts/release-version.sh)

case "$tag" in
  *[!A-Za-z0-9._-]*)
    printf 'release tag contains characters unsafe for asset names: %s\n' "$tag" >&2
    exit 1
    ;;
esac

printf '%s\n' "$tag"
