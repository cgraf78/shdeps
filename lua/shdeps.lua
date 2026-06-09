-- shdeps Lua API.
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
--
-- Runtime hosts that need to discover the installed shdeps module can load
-- `lua/shdeps/bootstrap.lua` and call `bootstrap.load(options)`.

local injected_source = ...

local function source_path()
  if type(injected_source) == "string" and injected_source:match("%.lua$") then
    return (injected_source:gsub("^@", ""):gsub("\\", "/"))
  end

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

local function source_dir()
  local source = source_path()
  return source and source:match("^(.*)/[^/]+$")
end

local dir = source_dir()
if not dir then
  error("shdeps Lua API requires debug source paths")
end

local api = dofile(dir .. "/shdeps/core.lua")
api.bootstrap = function(options)
  return dofile(dir .. "/shdeps/bootstrap.lua").load(options)
end
api.load = api.bootstrap

return api
