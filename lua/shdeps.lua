-- shdeps Lua API.
--
-- This module is intentionally a thin adapter over the shdeps CLI. Lua hosts
-- such as Neovim and WezTerm need to resolve files from shdeps-managed
-- dependencies, but they must not grow their own copy of shdeps config parsing
-- or install-root policy. All dependency ownership decisions stay in the Rust
-- binary; this module only provides a Lua-shaped runtime interface.
--
-- Public API:
--   local shdeps = dofile("/path/to/shdeps/lua/shdeps.lua")
--   local api = shdeps.new({ home = os.getenv("HOME") })
--   api.dep_root(name)          -> path string or nil
--   api.dep_path(name, rel)     -> path string or nil
--   api.dep_file(name, rel)     -> readable file path string or nil
--   api.env()                   -> table of environment overrides for child tools
--
-- The module also exposes default-object helpers:
--   shdeps.dep_root(name)
--   shdeps.dep_path(name, rel)
--   shdeps.dep_file(name, rel)
--   shdeps.env()

local M = {}

M.API_VERSION = 1

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

local function module_root()
  local source = source_path()
  if not source then
    return nil
  end
  return source:match("^(.*)/lua/shdeps%.lua$")
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

local function prepend_path(dir, path)
  if not dir or dir == "" then
    return path
  end
  if not path or path == "" then
    return dir
  end
  return dir .. ":" .. path
end

local function quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function required(value, name)
  if type(value) ~= "string" or value == "" then
    error("shdeps." .. name .. " requires a non-empty string", 3)
  end
  return value
end

local function first_readable(paths)
  for _, path in ipairs(paths) do
    if readable(path) then
      return path
    end
  end
  return nil
end

local function shdeps_bin(options)
  options = options or {}

  if readable(options.bin) then
    return options.bin
  end

  local explicit = os.getenv("SHDEPS_BIN")
  if readable(explicit) then
    return explicit
  end

  local bin_dir = options.bin_dir or os.getenv("SHDEPS_BIN_DIR")
  if type(bin_dir) == "string" and readable(bin_dir .. "/shdeps") then
    return bin_dir .. "/shdeps"
  end

  local root = options.root or module_root()
  if type(root) == "string" and root ~= "" then
    local sibling = first_readable({
      root .. "/shdeps",
      root .. "/target/debug/shdeps",
      root .. "/target/release/shdeps",
    })
    if sibling then
      return sibling
    end
  end

  local home = options.home
  if type(home) == "string" and readable(home .. "/.local/bin/shdeps") then
    return home .. "/.local/bin/shdeps"
  end

  local env_home = os.getenv("HOME")
  if type(env_home) == "string" and readable(env_home .. "/.local/bin/shdeps") then
    return env_home .. "/.local/bin/shdeps"
  end

  return "shdeps"
end

local function command_env(options, bin)
  options = options or {}
  local env = {}

  if type(options.env) == "table" then
    for key, value in pairs(options.env) do
      env[key] = value
    end
  end

  if type(options.home) == "string" and options.home ~= "" then
    env.HOME = options.home
  end

  if type(options.conf_dir) == "string" and options.conf_dir ~= "" then
    env.SHDEPS_CONF_DIR = options.conf_dir
  elseif type(options.home) == "string" and options.home ~= "" then
    env.SHDEPS_CONF_DIR = options.home .. "/.config/shdeps"
  elseif os.getenv("SHDEPS_CONF_DIR") then
    env.SHDEPS_CONF_DIR = os.getenv("SHDEPS_CONF_DIR")
  end

  local bin_dir = dirname(bin)
  if bin_dir and bin:sub(1, 1) == "/" then
    -- Some dependency-owned tools shell out to `shdeps` after Lua resolves
    -- their entrypoint. Prepending the selected binary directory keeps those
    -- child tools on the same shdeps installation instead of whatever PATH
    -- happened to contain before the editor or terminal started.
    env.PATH = prepend_path(bin_dir, env.PATH or os.getenv("PATH"))
  end

  return env
end

local function run(args, env)
  local parts = {}
  for _, key in ipairs({ "HOME", "SHDEPS_CONF_DIR", "PATH" }) do
    if env and env[key] then
      table.insert(parts, key .. "=" .. quote(env[key]))
    end
  end
  for _, arg in ipairs(args) do
    table.insert(parts, quote(arg))
  end

  local handle = io.popen(table.concat(parts, " ") .. " 2>/dev/null")
  if not handle then
    return nil
  end

  local output = handle:read("*a") or ""
  local ok = handle:close()
  if not ok then
    return nil
  end

  output = output:gsub("%s+$", "")
  if output == "" then
    return nil
  end
  return output
end

function M.new(options)
  options = options or {}
  local bin = shdeps_bin(options)
  local env = command_env(options, bin)

  return {
    dep_root = function(name)
      return run({ bin, "dep-root", required(name, "dep_root") }, env)
    end,

    dep_path = function(name, relative_path)
      return run({
        bin,
        "dep-path",
        required(name, "dep_path"),
        required(relative_path, "dep_path"),
      }, env)
    end,

    dep_file = function(name, relative_path)
      return run({
        bin,
        "dep-file",
        required(name, "dep_file"),
        required(relative_path, "dep_file"),
      }, env)
    end,

    env = function()
      local copy = {}
      for key, value in pairs(env) do
        copy[key] = value
      end
      return copy
    end,
  }
end

local default = M.new()

function M.dep_root(name)
  return default.dep_root(name)
end

function M.dep_path(name, relative_path)
  return default.dep_path(name, relative_path)
end

function M.dep_file(name, relative_path)
  return default.dep_file(name, relative_path)
end

function M.env()
  return default.env()
end

return M
