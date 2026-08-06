#!/usr/bin/env bash
set -euo pipefail

# Vendored from cgraf78/actions:release-scripts. Do not edit in consumer repos;
# CI verifies this file byte-matches the shared copy. The archive payload is
# declared in scripts/release.conf.

# shellcheck source=release-lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"

if [[ $# -ne 2 ]]; then
  printf 'usage: scripts/package-release.sh <rust-target> <asset-platform>\n' >&2
  exit 2
fi

release_load_config
release_package "$1" "$2"
