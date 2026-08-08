# Lua Loader API

This directory contains the Lua integration for consumers that need shdeps from
Neovim or other Lua hosts.

`shdeps.lua` is the public module. It loads the implementation under
`shdeps/`, locates a usable shdeps install, and exposes a small Lua API over the
same hidden CLI bridge used by other adapters. Keep host-specific setup out of
this layer; callers should configure paths through documented options or
environment variables.

`install.sh` publishes this entire directory through `$SHDEPS_LUA_DIR`
(default `~/.local/lib/shdeps`). It is a stable symlink to the active source,
developer, or release tree, so consumers can load
`~/.local/lib/shdeps/shdeps/bootstrap.lua` without copying Shdeps' install
discovery rules. The installer owns the symlink itself, but deliberately
refuses to replace a real file or directory at that path.

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
