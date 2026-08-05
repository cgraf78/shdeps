#!/usr/bin/env bash
set -euo pipefail

# Vendored from cgraf78/actions:release-scripts. Do not edit in consumer repos;
# CI verifies this file byte-matches the shared copy. Per-project knobs live in
# scripts/release.conf.

# shellcheck source=release-lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/release-lib.sh"

release_load_config
release_cut "$@"
