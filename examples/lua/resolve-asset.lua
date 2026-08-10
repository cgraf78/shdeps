-- Resolve one runtime asset without duplicating Shdeps install policy.
--
-- This module is intentionally host-neutral: Neovim, WezTerm, or another Lua
-- host can decide what to do with the returned file and child environment.
-- Shdeps remains responsible for choosing source versus release installs and
-- for proving the relative path stays below the dependency root.

local M = {}

local function required(value, name)
  if type(value) ~= "string" or value == "" then
    error("resolve-asset requires options." .. name, 3)
  end
  return value
end

function M.resolve(options)
  options = options or {}
  local home = options.home or os.getenv("HOME")
  local lua_dir = options.lua_dir or os.getenv("SHDEPS_LUA_DIR")
  if not lua_dir then
    home = required(home, "home")
    lua_dir = home .. "/.local/lib/shdeps"
  end

  -- Load the provider-owned bootstrap from its stable installed location.
  -- Passing the same options through keeps asset resolution and any child
  -- process on one Shdeps installation even when shell startup has not run.
  local bootstrap = dofile(lua_dir .. "/shdeps/bootstrap.lua")
  local api = bootstrap.new({
    home = home,
    conf_dir = options.conf_dir,
    bin = options.bin,
    bin_dir = options.bin_dir,
    root = options.root,
    env = options.env,
  })

  local dependency = required(options.dependency, "dependency")
  local relative_path = required(options.relative_path, "relative_path")
  local path = api.dep_file(dependency, relative_path)
  if not path then
    return nil, string.format("Shdeps could not resolve %s from %s", relative_path, dependency)
  end

  return path, api.env()
end

return M
