# Lua Loader API

This directory contains the Lua integration for consumers that need shdeps from
Neovim or other Lua hosts.

`shdeps.lua` is the public module. It loads the implementation under
`shdeps/`, locates a usable shdeps install, and exposes a small Lua API over the
same hidden CLI bridge used by other adapters. Keep host-specific setup out of
this layer; callers should configure paths through documented options or
environment variables.

## Files

- `shdeps.lua` is the module entrypoint for `require("shdeps")`.
- `shdeps/bootstrap.lua` resolves and bootstraps shdeps for Lua callers.
- `shdeps/core.lua` contains the reusable API functions after bootstrap has
  found the CLI.

## Tests

The shell tests exercise Lua behavior because they need to validate install
layout and sourceable bootstrap paths:

```sh
tests/shell/lua-api-test
tests/shell/lua-bootstrap-test
```

Run those tests when changing this directory or the hidden Rust API it calls.
