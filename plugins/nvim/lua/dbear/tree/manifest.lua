--- The mirror's manifest (`<root>/.dbear-mirror.json`): what each folder and file stands for, by
--- exact name, so nothing is ever read back from an escaped file name.
---
---   { version = 1, error?, connections = { {
---       id, name, kind, browses, dir, listed?, error?,
---       databases = { { name, dir, listed?, error?,
---         schemas = { { name, dir, tables = { { name, kind, file } } } } } } } } }
---
--- `dir`/`file` are the entry's file name in its parent folder. `listed` is when it was last listed
--- (os.time()), `error` why the last listing failed (the previous entries are kept).

local fs = require("dbear.tree.fs")

local M = {}

M.VERSION = 1

local data -- the manifest, loaded once per root

function M.reset()
  data = nil
end

--- The manifest (loaded from disk the first time; empty when there's none).
function M.get()
  if data then
    return data
  end
  local text = fs.read(fs.root() .. "/" .. fs.MANIFEST)
  local ok, decoded = pcall(vim.json.decode, text or "", { luanil = { object = true, array = true } })
  if ok and type(decoded) == "table" and decoded.version == M.VERSION then
    data = decoded
  else
    data = { version = M.VERSION }
  end
  data.connections = data.connections or {}
  for _, c in ipairs(data.connections) do
    c.databases = c.databases or {}
    for _, d in ipairs(c.databases) do
      d.schemas = d.schemas or {}
      for _, s in ipairs(d.schemas) do
        s.tables = s.tables or {}
      end
    end
  end
  return data
end

--- Whether the root holds a manifest.
function M.exists()
  return fs.kind(fs.root() .. "/" .. fs.MANIFEST) == "file"
end

function M.save()
  fs.write_manifest(vim.json.encode(M.get()))
end

function M.connection(id)
  for _, c in ipairs(M.get().connections) do
    if c.id == id then
      return c
    end
  end
end

function M.database(c, name)
  for _, d in ipairs(c and c.databases or {}) do
    if d.name == name then
      return d
    end
  end
end

local function by(list, field, value)
  for _, item in ipairs(list or {}) do
    if item[field] == value then
      return item
    end
  end
end

--- What the path relative to the root stands for: `{ level = "root"|"conn"|"db"|"schema"|"table",
--- conn, db, schema, table }`, or nil when the manifest doesn't know it.
function M.lookup(rel)
  if rel == "" then
    return { level = "root" }
  end
  local parts = vim.split(rel, "/", { plain = true })
  local c = by(M.get().connections, "dir", parts[1])
  if not c then
    return nil
  end
  local hit = { level = "conn", conn = c }
  if #parts == 1 then
    return hit
  end
  hit.db = by(c.databases, "dir", parts[2])
  if not hit.db then
    return nil
  end
  hit.level = "db"
  if #parts == 2 then
    return hit
  end
  hit.schema = by(hit.db.schemas, "dir", parts[3])
  if not hit.schema then
    return nil
  end
  hit.level = "schema"
  if #parts == 3 then
    return hit
  end
  hit.table = by(hit.schema.tables, "file", parts[4])
  if not hit.table or #parts > 4 then
    return nil
  end
  hit.level = "table"
  return hit
end

--- The path (below the root) of a connection, database, schema or table, as far as given.
function M.path(c, d, s, t)
  local parts = { fs.root(), c.dir }
  if d then
    parts[#parts + 1] = d.dir
  end
  if s then
    parts[#parts + 1] = s.dir
  end
  if t then
    parts[#parts + 1] = t.file
  end
  return table.concat(parts, "/")
end

return M
