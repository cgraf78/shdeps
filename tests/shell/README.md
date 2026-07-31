# Shell Integration Tests

These tests protect the Bash, installer, release, and Lua integration surfaces
around the Rust `shdeps` core.

## Layout

- `helpers.sh` owns shared fixture setup and assertions.
- `helpers-test` verifies shared temporary fixtures are reclaimed at exit.
- `install-sh-test` covers `install.sh`, including sourceable bootstrap mode.
- `installer-flow-test` exercises end-to-end install/update flows.
- `lua-api-test` and `lua-bootstrap-test` cover the Lua API and install
  discovery behavior.
- `release-scripts-test` verifies release helper scripts and archive shape.
- `shdeps-wrapper-test` covers the legacy `shdeps.sh` compatibility wrapper.

## Running

Build the Rust binary before wrapper tests when you want the shell tests to use
the local CLI:

```sh
cargo build --locked
SHDEPS_RUST_CLI=target/debug/shdeps tests/shell/shdeps-wrapper-test
```

The full shell suite used by CI is:

```sh
tests/shell/helpers-test
tests/shell/install-sh-test
tests/shell/completion-test
tests/shell/installer-flow-test
tests/shell/lua-api-test
tests/shell/lua-bootstrap-test
tests/shell/release-scripts-test
```
