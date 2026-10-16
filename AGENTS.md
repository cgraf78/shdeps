# AGENTS.md

## About

shdeps installs and updates CLI tools from declarative `*.conf` files. The Rust
crate and binary (`src/`) own all behavior. `shdeps.sh` is a thin sourceable
Bash wrapper over the hidden `shdeps __api ...` bridge. The README is the user
reference for config format, methods, environment variables, state, and extras;
link it instead of copying it here.

## Layout

- `src/`: CLI, install/update/prune logic, state, hooks, and the `__api` bridge.
  `src/extras.rs` holds the man page and completion discovery globs.
- `shdeps.sh`: sourceable wrapper. `install.sh`: curl-pipeable installer and
  sourceable `--bootstrap`; repo-owned here (not generated).
- `tests/*.rs`: Cargo integration tests. `tests/shell/*`: wrapper, installer,
  completion, Lua, and release-script suites (see `tests/shell/README.md`).
  `tests/termux-pidfd-signal.c`: Termux pidfd fixture.
- `scripts/`: release tooling. `smoke-install-bash32.sh` and `termux-ci.sh`
  are CI entry points.
- `docs/`: Rust port spec and plan, `checkout-lock-v1.md`, and
  `hook-contract.md` (hook, toolkit, and sudo retry invariants).
- `lua/`, `man/man1/shdeps.1`, `completions/`. `demo/record.sh` regenerates
  `demo/demo.gif`.

## Invariants

- Public `shdeps_*` functions in `shdeps.sh` stay one-line shims unless they
  must affect the caller's shell (`shdeps_dep_source`, changed-marker
  coordination). Put new behavior in Rust behind `__api`, covered by
  `tests/shell/shdeps-wrapper-test`.
- Hook execution and `shdeps_require_sudo` follow `docs/hook-contract.md`.
- `install.sh` must run under stock macOS Bash 3.2. The sourceable API and hook
  prelude require Bash 4.3+.
- MSRV is 1.85 (`rust-version` in `Cargo.toml`), and `Cargo.toml` sets
  `warnings = "deny"`, so a local warning fails the build.
- `tests/health.rs` asserts every health problem token appears in both
  `README.md` and `man/man1/shdeps.1`.
- dot pins shdeps' `install.sh` by revision, sha256, and wrapper ABI in its
  `support/shdeps.lock`. Changing `install.sh` or the wrapper ABI needs a dot
  repin.

## Testing

CI runs, and local runs should match:

```bash
cargo test --locked
cargo test --locked --test cli -- --ignored --test-threads=1
cargo build --release --locked
SHDEPS_RUST_CLI=target/release/shdeps tests/shell/shdeps-wrapper-test
tests/shell/helpers-test && tests/shell/install-sh-test &&
  tests/shell/install-interruption-test && tests/shell/completion-test &&
  tests/shell/examples-test && tests/shell/installer-flow-test &&
  tests/shell/lua-api-test && tests/shell/lua-bootstrap-test &&
  tests/shell/release-scripts-test
/bin/bash scripts/smoke-install-bash32.sh
```

- The ignored `cli` tests are wall-clock budgets; run them serially.
- The shared Rust gate also runs `cargo fmt --check`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`,
  `RUSTDOCFLAGS='-D missing-docs' cargo doc --locked --no-deps`,
  `cargo audit --file Cargo.lock`, and
  `cargo check --locked --all-targets --all-features` on the 1.85 toolchain.
- Termux CI compiles `tests/termux-pidfd-signal.c` with
  `-Wall -Wextra -Werror` and runs `scripts/termux-ci.sh` against the packaged
  release binary.

## CI and generated files

- CI's ShellCheck job fails on any shell program missing from
  `.github/shellcheck-files.txt` (untracked files included) and on stale rows;
  the local suites do not detect this. Before pushing, run
  `bash <actions@lock>/.github/actions/shellcheck-inventory/run.sh "$PWD" .github/shellcheck-files.txt`
  from a cgraf78/actions checkout at the SHA in `.github/cgraf78-actions.lock`.
- `.github/cgraf78-actions.lock` and every `uses: cgraf78/actions/...@sha`
  move together, only via `consumer-ci/sync.sh <this checkout>` run from
  cgraf78/actions at the locked SHA.
- Files listed in `scripts/.release-scripts.manifest` are vendored byte for
  byte from that commit; don't edit them here. `scripts/release.conf` and
  `scripts/release-smoke-hook.sh` are repo-owned.
- `retry-infrastructure.yml` keys on the workflow name `Tests`; don't rename it.

## Release

From a clean, green `main`, run `scripts/release.sh --push` (see the README's
Releasing section). The shared `rust-release` workflow publishes Linux musl,
macOS, and Android aarch64 and x86_64 archives.
