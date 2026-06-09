# Release Scripts

These scripts are maintainer tooling for cutting and smoke-testing shdeps
releases. Runtime install/update behavior belongs in Rust and `install.sh`, not
in this directory.

## Files

- `release-version.sh` computes the release version from repo state.
- `release-tag.sh` creates or validates the tag used for a release.
- `package-release.sh` builds the distributable archive layout.
- `smoke-release.sh` checks an unpacked release archive.
- `smoke-install-bash32.sh` verifies compatibility with older Bash installs.
- `release.sh` composes the release helpers for local release preparation.

## Expectations

Keep scripts deterministic and friendly to CI. If a script needs a generated
artifact, make the artifact path explicit and avoid depending on untracked local
state. Release archive shape changes should be covered by
`tests/shell/release-scripts-test`.
