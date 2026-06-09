# shdeps Rust Core

This directory owns the compiled `shdeps` CLI and the reusable Rust library
surface behind it. Keep dependency resolution, update policy, state tracking,
and install mechanics here; Bash and Lua entrypoints should stay thin adapters
over this implementation.

## Boundaries

- `main.rs` is only the binary entrypoint. Shared behavior belongs in
  `lib.rs` and the modules it exposes.
- `cli.rs` owns argument parsing and output routing. It should translate user
  intent into typed calls rather than embedding install/update policy.
- `api.rs` owns the hidden bridge used by shell and Lua loaders. Keep this
  surface narrow and well tested because external integrations depend on its
  stability even though it is not the primary user interface.
- `manifest.rs`, `method.rs`, and `config.rs` own dependency declarations and
  config interpretation. Consumers should not duplicate this vocabulary.
- `update*.rs`, `github*.rs`, `pkg.rs`, and `repo.rs` own method-specific
  update/install behavior.
- `state.rs`, `stamp.rs`, `install_metadata.rs`, `link_state.rs`, and
  `dep_links.rs` own durable files written under shdeps-managed directories.

## Design Notes

`shdeps self-update` is the single building block for updating shdeps itself.
Bootstrap scripts and host projects should call that command instead of
recreating release, source checkout, or git-repo update logic.

HTTP and GitHub access should flow through the shared helper modules so retry,
cache, rate-limit, and error handling stay consistent across install methods.
New persistent strings, method names, or API keys should be centralized in the
module that owns the underlying concept.

## Tests

Rust unit and integration tests cover the core API:

```sh
cargo test --locked
```

Shell compatibility and installer behavior live in `tests/shell/`. Update those
tests whenever a Rust change affects `install.sh`, `shdeps.sh`, Lua loading, or
release packaging.
