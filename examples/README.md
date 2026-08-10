# Examples

These examples are loaded by the test suite through Shdeps' real parsers and
public APIs. They use public projects or synthetic names and contain no
machine-specific inventory.

- `deps.conf` covers every dependency method and the optional command, alias,
  and filter columns.
- `hooks.d/example-hook.sh` demonstrates a custom dependency and a post-install
  completion hook. Copy only the lifecycle functions a dependency needs.
- `lua/resolve-asset.lua` demonstrates the provider-managed Lua bootstrap,
  validated `dep_file` lookup, and the environment to pass to a child tool.

For example, a Lua host can load the resolver and keep host-specific behavior
outside Shdeps:

```lua
local resolver = dofile("/path/to/resolve-asset.lua")
local setup, child_env = resolver.resolve({
  dependency = "cgraf78/termnav",
  relative_path = "lib/termnav/nvim/setup.lua",
})
if not setup then
  return
end
dofile(setup).setup()
```

The returned `child_env` is useful when the resolved tool starts a child
process that may invoke `shdeps`; merge it with the host's normal environment
according to that host's process API.
