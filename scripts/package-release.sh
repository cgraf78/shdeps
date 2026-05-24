#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  printf 'usage: scripts/package-release.sh <rust-target> [asset-platform]\n' >&2
  exit 2
fi

target=$1
asset_platform=${2:-$target}

case "$asset_platform" in
  '' | *[!A-Za-z0-9._-]*)
    printf 'asset platform contains characters unsafe for asset names: %s\n' "$asset_platform" >&2
    exit 2
    ;;
esac

tag=$(scripts/release-tag.sh)
commit=${SHDEPS_BUILD_COMMIT:-}
if [[ -z "$commit" ]]; then
  commit=$(git rev-parse --short HEAD)
fi
if [[ ! "$commit" =~ ^[0-9a-fA-F]{7,}$ ]]; then
  printf 'build commit must be a concrete git hash, got %s\n' "$commit" >&2
  exit 1
fi

asset="shdeps-${tag}-${asset_platform}.tar.gz"
dist_dir=dist
staging=$(mktemp -d)

cleanup() {
  rm -rf "$staging"
}
trap cleanup EXIT

# Release archives are consumed by bootstrap paths that may not have a Rust
# toolchain or a source checkout. Package only the executable plus the stable
# Bash/sourceable compatibility surface and docs that installer/self-update
# code knows how to activate.
cargo build --release --locked --target "$target"

install -m 0755 "target/${target}/release/shdeps" "$staging/shdeps"
# `shdeps.sh` is now the Rust-era sourceable wrapper. Keep release archives
# pointed at that single public wrapper path so source checkouts and packaged
# installs expose the same Bash API surface.
install -m 0644 shdeps.sh "$staging/shdeps.sh"
install -m 0755 install.sh "$staging/install.sh"
install -m 0644 README.md "$staging/README.md"
install -m 0644 LICENSE "$staging/LICENSE"

mkdir -p "$staging/man/man1" "$staging/completions"
install -m 0644 man/man1/shdeps.1 "$staging/man/man1/shdeps.1"
install -m 0644 completions/shdeps.bash "$staging/completions/shdeps.bash"
install -m 0644 completions/shdeps.zsh "$staging/completions/shdeps.zsh"
install -m 0644 completions/shdeps.fish "$staging/completions/shdeps.fish"

# Include install metadata in the archive so an extracted release can be
# identified without guessing from filesystem shape. Activation code may rewrite
# or enrich this file at install time, but the packaged copy gives bundled-mode
# installer tests a concrete release identity to validate.
cat >"$staging/.shdeps-install.json" <<EOF
{
  "schema": 1,
  "method": "release",
  "artifact_platform": "$asset_platform",
  "tag": "$tag",
  "commit": "$commit",
  "repo": "cgraf78/shdeps"
}
EOF

mkdir -p "$dist_dir"
tar -C "$staging" -czf "${dist_dir}/${asset}" .

# GNU coreutils and macOS expose different checksum commands. Prefer
# `sha256sum` when available, but keep local/macOS release dry-runs independent
# of Homebrew bootstrap state.
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist_dir" && sha256sum "$asset") >"${dist_dir}/${asset}.sha256"
else
  (cd "$dist_dir" && shasum -a 256 "$asset") >"${dist_dir}/${asset}.sha256"
fi

printf '%s\n' "${dist_dir}/${asset}"
