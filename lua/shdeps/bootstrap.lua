-- shdeps Lua bootstrap helper.
--
-- This module discovers a shdeps Lua API module from common source-checkout and
-- release-install locations, then returns that loaded module. It is intended
-- for Lua hosts such as Neovim and WezTerm that start before shell init has
-- normalized PATH.

local M = {}

local function source_path()
  if type(debug) ~= "table" or type(debug.getinfo) ~= "function" then
    return nil
  end

  local info = debug.getinfo(1, "S")
  local source = info and info.source or ""
  if source == "" then
    return nil
  end
  return (source:gsub("^@", ""):gsub("\\", "/"))
end

local function source_root()
  local source = source_path()
  return source and source:match("^(.*)/lua/shdeps/bootstrap%.lua$")
end

local function readable(path)
  if type(path) ~= "string" or path == "" then
    return false
  end

  local file = io.open(path, "r")
  if not file then
    return false
  end
  file:close()
  return true
end

local function dirname(path)
  if type(path) ~= "string" or path == "" then
    return nil
  end
  return path:match("^(.*)/[^/]+$")
end

local function add_root(candidates, seen, root)
  if type(root) ~= "string" or root == "" then
    return
  end

  local path = root .. "/lua/shdeps.lua"
  if seen[path] then
    return
  end

  seen[path] = true
  table.insert(candidates, path)
end

local function add_path(candidates, seen, path)
  if type(path) ~= "string" or path == "" or seen[path] then
    return
  end

  seen[path] = true
  table.insert(candidates, path)
end

local function candidates(options)
  options = options or {}
  local home = options.home
  local env_home = os.getenv("HOME")
  local result = {}
  local seen = {}

  add_path(result, seen, options.lua)
  add_path(result, seen, os.getenv("SHDEPS_LUA"))

  add_root(result, seen, options.root)

  local lib = options.lib or os.getenv("SHDEPS_LIB")
  add_root(result, seen, dirname(lib))

  local git_dev_dir = options.git_dev_dir or os.getenv("SHDEPS_GIT_DEV_DIR")
  add_root(result, seen, git_dev_dir and (git_dev_dir .. "/shdeps"))
  add_root(result, seen, type(home) == "string" and (home .. "/git/shdeps") or nil)
  add_root(result, seen, type(env_home) == "string" and (env_home .. "/git/shdeps") or nil)

  add_root(result, seen, options.dir)
  add_root(result, seen, os.getenv("SHDEPS_DIR"))
  add_root(result, seen, type(home) == "string" and (home .. "/.local/share/shdeps") or nil)
  add_root(result, seen, type(env_home) == "string" and (env_home .. "/.local/share/shdeps") or nil)
  add_root(result, seen, source_root())

  return result
end

function M.paths(options)
  return candidates(options)
end

function M.load(options)
  for _, path in ipairs(candidates(options)) do
    if readable(path) then
      return dofile(path)
    end
  end

  error("shdeps Lua API not found; expected lua/shdeps.lua under shdeps root")
end

function M.new(options)
  return M.load(options).new(options)
end

return M
