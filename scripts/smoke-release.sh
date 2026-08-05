#!/usr/bin/env bash
set -euo pipefail

# Vendored from cgraf78/actions:release-scripts. Do not edit in consumer repos;
# CI verifies this file byte-matches the shared copy. Repo-specific runtime
# assertions belong in scripts/release-smoke-hook.sh.

# shellcheck source=release-lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"

if [[ $# -ne 1 ]]; then
  printf 'usage: scripts/smoke-release.sh <asset-platform>\n' >&2
  exit 2
fi

release_load_config
release_smoke "$1"
