--- `dbear://` buffer names:
---
---   dbear://                                  saved connections
---   dbear://<conn>/                           a connection: its databases, or its tables
---   dbear://<conn>/<db>/                      a database's tables
---   dbear://<conn>/[<db>/]<schema>.<table>    a table's rows (`?where=…&sort=a,-b` optional)
---
--- Components are percent-encoded where they hold `%`, `/`, `?`, `&`, `#`, spaces (and `.` in a
--- schema). `<conn>` is a saved connection's name or id.

local M = {}

M.SCHEME = "dbear://"

local function encode(s, extra)
  local pattern = "[%%/?&#%s" .. (extra or "") .. "]"
  return (s:gsub(pattern, function(c) return string.format("%%%02X", c:byte()) end))
end

local function decode(s)
  return (s:gsub("%%(%x%x)", function(h) return string.char(tonumber(h, 16)) end))
end

local function parse_sort(s)
  local sort = {}
  for part in s:gmatch("[^,]+") do
    local desc = part:sub(1, 1) == "-"
    sort[#sort + 1] = { column = decode(desc and part:sub(2) or part), descending = desc }
  end
  return sort
end

--- A buffer name → `{ kind = "root"|"conn"|"db"|"table", conn, db, schema, table, filter, sort }`, or nil.
function M.parse(name)
  if not vim.startswith(name, M.SCHEME) then
    return nil
  end
  local rest = name:sub(#M.SCHEME + 1)
  local path, query = rest:match("^([^?]*)%??(.*)$")
  if path == "" then
    return { kind = "root" }
  end
  if path == "results" then -- the results split
    return nil
  end
  local parts = vim.split(path, "/", { plain = true })
  local spec = { conn = decode(parts[1]) }
  local leaf
  if #parts == 1 or (#parts == 2 and parts[2] == "") then
    spec.kind = "conn"
  elseif #parts == 2 then
    leaf = parts[2]
  elseif #parts == 3 and parts[3] == "" then
    spec.kind, spec.db = "db", decode(parts[2])
  elseif #parts == 3 then
    spec.db, leaf = decode(parts[2]), parts[3]
  else
    return nil
  end
  if leaf then
    spec.kind = "table"
    local schema, table = leaf:match("^([^.]*)%.(.+)$")
    if schema then
      spec.schema, spec.table = decode(schema), decode(table)
    else
      spec.table = decode(leaf)
    end
    for k, v in query:gmatch("([^&=]+)=([^&]*)") do
      if k == "where" and v ~= "" then
        spec.filter = decode(v)
      elseif k == "sort" and v ~= "" then
        spec.sort = parse_sort(v)
      end
    end
  end
  return spec
end

--- The buffer name of `spec`.
function M.format(spec)
  if spec.kind == "root" then
    return M.SCHEME
  end
  local s = M.SCHEME .. encode(spec.conn) .. "/"
  if spec.db then
    s = s .. encode(spec.db) .. "/"
  end
  if spec.kind ~= "table" then
    return s
  end
  s = s .. (spec.schema and encode(spec.schema, ".") .. "." or "") .. encode(spec.table)
  local query = {}
  if spec.filter and spec.filter ~= "" then
    query[#query + 1] = "where=" .. encode(spec.filter, "=")
  end
  if spec.sort and #spec.sort > 0 then
    local parts = {}
    for _, k in ipairs(spec.sort) do
      parts[#parts + 1] = (k.descending and "-" or "") .. encode(k.column, ",=")
    end
    query[#query + 1] = "sort=" .. table.concat(parts, ",")
  end
  return #query > 0 and s .. "?" .. table.concat(query, "&") or s
end

--- The spec one level up.
function M.parent(spec)
  if spec.kind == "table" then
    return spec.db and { kind = "db", conn = spec.conn, db = spec.db } or { kind = "conn", conn = spec.conn }
  elseif spec.kind == "db" then
    return { kind = "conn", conn = spec.conn }
  end
  return { kind = "root" }
end

return M
