--- Listing into the mirror: connection folders from the store (`connections.list`, no network),
--- database folders (`databases.list`), schema folders and table files (`schemas.list` +
--- `columns.list`). A listing that fails records its error in the manifest and changes nothing
--- else on disk. Folders are reconciled: entries are renamed when their name changed (so a
--- renamed connection keeps what was listed), stale ones removed, new ones created read-only.
--- A connection folder renamed by hand (oil, `mv`) is recognized by its identity and renames the
--- connection in the store (`connections.rename`) before the root is reconciled (see `scan`).

local fs = require("dbear.tree.fs")
local manifest = require("dbear.tree.manifest")
local names = require("dbear.tree.names")

local M = {}

--- Keys being listed now ("root", "c:<id>", "d:<id>\0<db>") → true.
M.busy = {}
--- Keys listed (or tried: a failure isn't retried until `:DbearSync`) in this Neovim session.
M.listed = {}
--- Called when a listing starts (`false`) and ends (`true`: folders may have changed).
M.on_change = function(_) end
--- Called when the listing of `key` starts, and when it ends (however it ends) with its error.
M.on_start = function(_) end
M.on_finish = function(_, _) end

local waiters = {}

local function dbear()
  return require("dbear")
end

local function notify(msg, level)
  vim.notify("dbear: " .. msg, level or vim.log.levels.INFO)
end

local function message(err)
  return type(err) == "table" and (err.message or vim.inspect(err)) or tostring(err)
end

function M.conn_key(id)
  return "c:" .. id
end

function M.db_key(id, db)
  return "d:" .. id .. "\0" .. db
end

--- How to list a failed listing again: `(:DbearSync[ <conn>] retries)`.
function M.retry_hint(c)
  return "(:DbearSync " .. (c and c.name .. " " or "") .. "retries)"
end

--- Runs `work(done)` once for `key`; callers that come while it runs get the same result.
local function once(key, cb, work)
  cb = cb or function() end
  if M.busy[key] then
    table.insert(waiters[key], cb)
    return
  end
  M.busy[key] = true
  waiters[key] = { cb }
  M.on_start(key)
  M.on_change(false)
  -- `done(err, msg)`: `msg` is the error message, shown once its progress has ended.
  work(function(err, msg)
    M.busy[key] = nil
    local list = waiters[key]
    waiters[key] = nil
    M.listed[key] = true
    M.on_finish(key, err)
    if msg then
      notify(msg, vim.log.levels.ERROR)
    end
    M.on_change(true)
    for _, f in ipairs(list) do
      f(err)
    end
  end)
end

--- Applies `fn` (which changes the disk and the manifest); errors become `done(err)`.
local function apply(done, fn)
  local ok, err = pcall(fn)
  if not ok then
    return done(err, tostring(err))
  end
  done(nil)
end

-- MARK: Folders

--- Makes folder `dir` hold exactly `items` (`{ name, ext?, type = "dir"|"file", content?, old? }`,
--- `file` assigned by `names.assign`), keeping `keep` (names). Folders whose name changed (`old`)
--- are renamed, keeping what's in them; others are created empty. Sets each item's final `file`.
local function reconcile(dir, items, opts)
  opts = opts or {}
  local keep = opts.keep or {}
  fs.ensure_dir(dir)
  fs.with_writable(dir, function()
    -- Renames, in two steps so names can swap.
    local moving = {}
    for _, it in ipairs(items) do
      if it.type == "dir" and it.old and it.old ~= it.file and fs.kind(dir .. "/" .. it.old) == "directory" then
        local tmp = it.old .. "%~tmp"
        fs.rename(dir .. "/" .. it.old, dir .. "/" .. tmp)
        moving[#moving + 1] = { it, tmp }
      end
    end
    for _, m in ipairs(moving) do
      local to = dir .. "/" .. m[1].file
      if fs.kind(to) then
        fs.remove(to)
      end
      fs.rename(dir .. "/" .. m[2], to)
    end
    -- Stale entries (and entries of the wrong type).
    local want, taken = {}, {}
    for _, it in ipairs(items) do
      want[it.file] = it
      taken[names.fold(it.file)] = true
    end
    for name in fs.list(dir) do
      local it = want[name]
      local is_dir = fs.kind(dir .. "/" .. name) == "directory"
      if not keep[name] and (not it or (it.type == "dir") ~= is_dir or (opts.fresh and is_dir)) then
        fs.remove(dir .. "/" .. name)
      end
    end
    -- New and changed entries. Two names the file system can't tell apart (beyond case, e.g.
    -- Unicode normalization) land on one inode: the later one gets a `%~N` name.
    local claimed = {}
    for _, it in ipairs(items) do
      local path = dir .. "/" .. it.file
      local ino = fs.inode(path)
      if ino and claimed[ino] then
        it.file = names.bump(it.file, it.ext or "", taken)
        taken[names.fold(it.file)] = true
        path = dir .. "/" .. it.file
      end
      if it.type == "dir" then
        fs.mkdir(path)
        fs.chmod(path, fs.DIR_MODE)
      else
        fs.write(path, it.content, opts.fresh)
      end
      claimed[fs.inode(path)] = true
    end
  end)
end

-- MARK: Table files

local function clean(s)
  return (tostring(s):gsub("%c", function(c) return string.format("\\x%02X", c:byte()) end))
end

local function pad(s, w)
  return s .. string.rep(" ", w - vim.api.nvim_strwidth(s))
end

--- The first line of a table file: `#dbear-mirror v1 {json}` (keys in a fixed order).
function M.header(c, database, schema, t)
  local j = vim.json.encode
  return string.format(
    '#dbear-mirror v1 {"connection":%s,"database":%s,"browses":%s,"schema":%s,"table":%s,"kind":%s}',
    j(c.id), j(database), j(c.browses and true or false), j(schema), j(t.name), j(t.kind)
  )
end

--- Reads a table file's header: `{ connection, database, browses, schema, table, kind }` or nil.
function M.parse_header(line)
  local json = (line or ""):match("^#dbear%-mirror v1 (%b{})")
  if not json then
    return nil
  end
  local ok, h = pcall(vim.json.decode, json)
  if not ok or type(h) ~= "table" or type(h.connection) ~= "string" or type(h.table) ~= "string" then
    return nil
  end
  return h
end

local function file_content(c, d, database, schema, t, cols, cols_err)
  local lines = {
    M.header(c, database, schema, t),
    string.format("-- %s: %s in %s / %s / %s", clean(t.name), t.kind, clean(c.name), clean(d.label or database), clean(schema)),
    "-- Open this file in Neovim (dbear) to browse and edit its rows. Columns:",
  }
  if cols_err then
    lines[#lines + 1] = "-- (columns unavailable: " .. clean(cols_err) .. ")"
  else
    local wn, wt = 0, 0
    for _, col in ipairs(cols or {}) do
      wn = math.max(wn, vim.api.nvim_strwidth(clean(col.name)))
      wt = math.max(wt, vim.api.nvim_strwidth(clean(col.type_name or "")))
    end
    for _, col in ipairs(cols or {}) do
      local flags = (col.is_primary_key and "PK  " or "    ") .. (col.is_nullable and "NULL" or "NOT NULL")
      lines[#lines + 1] = pad(clean(col.name), wn) .. "  " .. pad(clean(col.type_name or ""), wt) .. "  " .. flags
    end
  end
  return table.concat(lines, "\n") .. "\n"
end

-- MARK: Migration

--- Called with the old and new path of a file or folder that moved (its buffers follow it).
M.on_moved = function(_, _) end

--- Table files in the order they're named in a schema folder, each given its `file`.
local function table_items(tables)
  table.sort(tables, function(a, b) return a.name < b.name end)
  return names.assign(tables)
end

--- Mirrors written by manifest version 1 named table files `<table>.table` and `<view>.view`.
--- Renames them in place to today's names (no suffix), offline: each file's content (header and
--- columns) is kept, only its name changes, so databases that were never listed stay so and
--- nothing connects. Files that are gone or have no header are left out, and their database is
--- listed again the next time it's entered. Anything else in a schema folder (the old names) is
--- removed by the reconcile. Rows buffers open on an old name follow their file (`on_moved`).
--- Runs once, before any listing; a no-op on a current or missing manifest.
function M.migrate()
  if not manifest.exists() then
    return
  end
  local man = manifest.get()
  if not man.migrate_from then
    return
  end
  local moves = {}
  for _, c in ipairs(man.connections) do
    for _, d in ipairs(c.databases) do
      local ok, err = pcall(function()
        local lost = false
        for _, s in ipairs(d.schemas) do
          local dir = manifest.path(c, d, s)
          if fs.kind(dir) == "directory" then
            local items = {}
            for _, t in ipairs(s.tables) do
              local old = t.file and dir .. "/" .. t.file
              local content = old and fs.kind(old) == "file" and fs.read(old)
              if content and M.parse_header(content:match("^[^\n]*")) then
                items[#items + 1] = { name = t.name, kind = t.kind, type = "file", content = content, from = old }
              else
                lost = true
              end
            end
            table_items(items)
            -- Open table files keep their mode under their new name.
            local pairs_ = {}
            for _, it in ipairs(items) do
              pairs_[#pairs_ + 1] = { it.from, dir .. "/" .. it.file }
            end
            fs.remap(pairs_)
            reconcile(dir, items)
            s.tables = {}
            for _, it in ipairs(items) do
              s.tables[#s.tables + 1] = { name = it.name, kind = it.kind, file = it.file }
              if it.from ~= dir .. "/" .. it.file then
                moves[#moves + 1] = { it.from, dir .. "/" .. it.file }
              end
            end
          elseif #s.tables > 0 then
            lost = true
          end
        end
        if lost then
          d.listed = nil
          if not c.browses then
            c.listed = nil
          end
        end
      end)
      if not ok then
        -- Listed again when entered (its reconcile removes what's left of the old names).
        notify("migrating " .. tostring(c.name) .. " / " .. tostring(d.name) .. ": " .. tostring(err), vim.log.levels.WARN)
        d.listed = nil
        if not c.browses then
          c.listed = nil
        end
      end
    end
  end
  man.migrate_from = nil
  man.version = manifest.VERSION
  manifest.save()
  -- Buffers follow (in an order where each new name is free: `x.table` → `x` before
  -- `x.table.table` → `x.table`).
  local left = moves
  while #left > 0 do
    local next_left = {}
    local froms = {}
    for _, m in ipairs(left) do
      froms[m[1]] = true
    end
    for _, m in ipairs(left) do
      if froms[m[2]] then
        next_left[#next_left + 1] = m
      else
        M.on_moved(m[1], m[2])
      end
    end
    if #next_left == #left then
      break
    end
    left = next_left
  end
end

local function migrate()
  local ok, err = pcall(M.migrate)
  if not ok then
    notify(tostring(err), vim.log.levels.ERROR)
  end
end

-- MARK: Listings

local function request(method, params, cb)
  dbear().rpc():request(method, params, cb)
end

-- MARK: Connection folders and renames

--- Where each renamed connection folder was before (by id), until its buffers follow it.
local moved = {}

--- Renames saved connection `id` (`connections.rename`: the store's name only). `old` is its name
--- before, for the message. `cb(err, renamed)`.
function M.rename(id, old, name, cb)
  request("connections.rename", { connection = id, name = name }, function(err, res)
    if err then
      notify(string.format("can't rename “%s” to “%s”: %s", old, name, message(err)), vim.log.levels.ERROR)
      return cb and cb(err)
    end
    if res.previous_name ~= res.name then -- another Neovim may have renamed it already
      notify(string.format("renamed “%s” to “%s”", res.previous_name, res.name))
    end
    if cb then
      cb(nil, res)
    end
  end)
end

--- Looks at the root for connection folders renamed outside the mirror's own code (oil, `mv`):
--- a folder is the connection whose identity (inode + creation time, recorded at each listing) it
--- has, whatever its name now; nothing is ever read as a delete plus a create. Each one found gets
--- its manifest `dir` set to where it is now. Returns those whose folder name now stands for
--- another name (`{ c, name }`: the store must be renamed) and the connections whose folder is
--- gone (deleted from outside: they're re-created empty, the store is left alone).
local function scan(man)
  local root = fs.root()
  local at = {}
  for name in fs.list(root) do
    local id = fs.identity(root .. "/" .. name)
    if id then
      at[id] = name
    end
  end
  local renames, lost = {}, {}
  for _, c in ipairs(man.connections) do
    local now = c.ident and at[c.ident]
    if now and now ~= c.dir then
      moved[c.id] = moved[c.id] or c.dir
      c.dir = now
      local name, why = names.parse(now)
      if dbear().config.tree.rename == false then
        if name ~= c.name then
          notify("renaming connections from the mirror is off (tree.rename = false): “" .. now .. "” goes back to “" .. c.name .. "”", vim.log.levels.WARN)
        end
      elseif not name then
        notify(string.format("can't rename “%s” to “%s”: %s", c.name, now, why), vim.log.levels.ERROR)
      elseif name ~= c.name then
        renames[#renames + 1] = { c = c, name = name }
      end
    elseif not now and c.dir and not (c.ident == nil and fs.kind(root .. "/" .. c.dir) == "directory") then
      lost[#lost + 1] = c
    end
  end
  return renames, lost
end

--- Renames the store for each of `renames`, one after the other, then `cb()`. A failure is shown
--- and leaves the store as it was: the next reconcile moves the folder back.
local function rename_all(renames, cb)
  local i = 0
  local function step()
    i = i + 1
    local r = renames[i]
    if not r then
      return cb()
    end
    M.rename(r.c.id, r.c.name, r.name, function() step() end)
  end
  step()
end

--- Connection folders from the store. Folders renamed by hand are renamed in the store first
--- (see `scan`); anything else at the root that isn't a connection's folder is removed, and a
--- connection folder deleted from outside comes back (empty, listed again when entered). `cb(err)`.
function M.root(opts, cb)
  opts = opts or {}
  migrate()
  once("root", cb, function(done)
    local ok, renames = pcall(scan, manifest.get())
    rename_all(ok and renames or {}, function()
      request("connections.list", nil, function(err, res)
        if err then
          manifest.get().error = message(err)
          pcall(manifest.save)
          return done(err, "can't list the connections: " .. message(err) .. " " .. M.retry_hint())
        end
        local again = false
        apply(done, function()
          local man = manifest.get()
          -- Renamed again while the store was being renamed or listed: don't move it back, list again.
          local more, lost = scan(man)
          if #more > 0 then
            again = true
            return manifest.save()
          end
          local old, known = {}, {}
          for _, c in ipairs(man.connections) do
            old[c.id] = c
          end
          for _, c in ipairs(lost) do
            notify(string.format("“%s” is still saved: connections can't be deleted from the mirror, its folder is back", c.name), vim.log.levels.WARN)
            local there = fs.root() .. "/" .. c.dir
            local taken = false
            for _, x in ipairs(man.connections) do
              taken = taken or (x ~= c and x.dir == c.dir)
            end
            if fs.kind(there) and not taken then -- something else took its name: not its folder
              fs.with_writable(fs.root(), function() fs.remove(there) end)
            end
            c.databases, c.listed, c.error, c.dir, c.ident = {}, nil, nil, nil, nil
            for key in pairs(M.listed) do
              if key == M.conn_key(c.id) or vim.startswith(key, "d:" .. c.id .. "\0") then
                M.listed[key] = nil
              end
            end
          end
          for _, c in ipairs(man.connections) do
            if c.dir then
              known[c.dir] = true
            end
          end
          local list, items = {}, {}
          for _, l in ipairs(res.connections) do
            if not vim.startswith(l.id, "url:") then -- URLs are never mirrored
              local c = old[l.id] or { id = l.id, databases = {} }
              c.name, c.kind, c.browses = l.name, l.kind, l.browses_databases and true or false
              list[#list + 1] = c
              items[#items + 1] = { name = c.name, type = "dir", old = c.dir, c = c }
            end
          end
          names.assign(items, { [names.fold(fs.MANIFEST)] = true })
          local keep = { [fs.MANIFEST] = true, [fs.MANIFEST .. ".tmp"] = true }
          local strays = {}
          for name in fs.list(fs.root()) do
            if not keep[name] and not known[name] and not name:find("%~tmp", 1, true) then
              strays[#strays + 1] = name
            end
          end
          reconcile(fs.root(), items, { keep = keep })
          if #strays > 0 then
            table.sort(strays)
            notify("removed “" .. table.concat(strays, "”, “") .. "” from the mirror's root: only renaming connection folders works there", vim.log.levels.WARN)
          end
          for _, it in ipairs(items) do
            local c = it.c
            local was = moved[c.id] or it.old
            moved[c.id] = nil
            c.dir = it.file
            c.ident = fs.identity(fs.root() .. "/" .. it.file)
            if was and was ~= it.file then
              M.on_moved(fs.root() .. "/" .. was, fs.root() .. "/" .. it.file)
            end
          end
          man.connections, man.error = list, nil
          manifest.save()
        end)
        if again then
          vim.schedule(function() M.root_again(opts) end)
        end
      end)
    end)
  end)
end

--- `M.root`, but after the listing in progress if there's one (whose result may predate a rename).
function M.root_again(opts, cb)
  if M.busy.root then
    table.insert(waiters.root, function() M.root_again(opts, cb) end)
    return
  end
  M.root(opts, cb)
end

local function db_items(c)
  local items = {}
  for _, d in ipairs(c.databases) do
    items[#items + 1] = { name = d.label or d.name, type = "dir", old = d.dir, d = d }
  end
  names.assign(items)
  return items
end

local function place_databases(c)
  local items = db_items(c)
  reconcile(manifest.path(c), items)
  for _, it in ipairs(items) do
    it.d.dir = it.file
  end
end

--- Lists one database (`db` nil: a connection that doesn't browse databases, its own).
function M.database(id, db, opts, cb)
  opts = opts or {}
  migrate()
  local c = manifest.connection(id)
  if not c then
    return cb and cb("unknown connection " .. id)
  end
  local key = db and M.db_key(id, db) or M.conn_key(id)
  once(key, cb, function(done)
    local schemas, columns, failed, cols_err
    local pending = 2
    local function finish()
      pending = pending - 1
      if pending > 0 then
        return
      end
      c = manifest.connection(id)
      if not c then
        return done("connection " .. id .. " is gone")
      end
      local d = db and manifest.database(c, db)
      if failed then
        local msg = message(failed)
        if d then
          d.error = msg
        else
          c.error = msg
        end
        pcall(manifest.save)
        return done(failed, "can't list " .. c.name .. (db and " / " .. db or "") .. ": " .. msg .. " " .. M.retry_hint(c))
      end
      apply(done, function()
        local database = schemas.target.database
        if not d then
          d = manifest.database(c, database) or { name = database, schemas = {} }
          if not db then
            -- Its own database: named after the file for SQLite, "default" when it has no name.
            if c.kind == "sqlite" then
              d.label = vim.fs.basename(database)
            elseif database == "" then
              d.label = "default"
            end
            c.databases = { d }
          else
            table.insert(c.databases, d)
          end
          place_databases(c)
        end
        local by_table = {}
        for _, tc in ipairs(columns and columns.tables or {}) do
          by_table[tc.schema .. "\0" .. tc.table] = tc.columns
        end
        local dir = manifest.path(c, d)
        local old = {}
        for _, s in ipairs(d.schemas) do
          old[s.name] = s.dir
        end
        local items = {}
        for _, s in ipairs(schemas.schemas) do
          items[#items + 1] = { name = s.name, type = "dir", old = old[s.name], s = s }
        end
        names.assign(items)
        reconcile(dir, items, { fresh = opts.fresh })
        local list = {}
        for _, it in ipairs(items) do
          local s = it.s
          local titems = {}
          for _, t in ipairs(s.tables) do
            local cols = by_table[s.name .. "\0" .. t.name]
            titems[#titems + 1] = {
              name = t.name,
              type = "file",
              content = file_content(c, d, database, s.name, t, cols, cols_err or (not cols and "not listed" or nil)),
              t = t,
            }
          end
          table_items(titems)
          reconcile(dir .. "/" .. it.file, titems, { fresh = opts.fresh })
          local entry = { name = s.name, dir = it.file, tables = {} }
          for _, ti in ipairs(titems) do
            entry.tables[#entry.tables + 1] = { name = ti.t.name, kind = ti.t.kind, file = ti.file }
          end
          list[#list + 1] = entry
        end
        d.schemas, d.listed, d.error = list, os.time(), nil
        if not db then
          c.listed, c.error = os.time(), nil
        end
        manifest.save()
      end)
    end
    local on = { connection = id, database = db }
    request("schemas.list", vim.tbl_extend("force", on, { refresh = true }), function(err, res)
      failed, schemas = failed or err, res
      finish()
    end)
    request("columns.list", on, function(err, res)
      cols_err, columns = err and message(err) or nil, res
      finish()
    end)
  end)
end

--- Lists a connection: its databases (a connection that browses them), else its own database.
--- `opts.deep`: then lists every database too (`"listed"`: those listed before).
function M.connection(id, opts, cb)
  opts = opts or {}
  migrate()
  local c = manifest.connection(id)
  if not c then
    return cb and cb("unknown connection " .. id)
  end
  if not c.browses then
    return M.database(id, nil, opts, cb)
  end
  local function deep(err)
    if err or not opts.deep then
      return cb and cb(err)
    end
    c = manifest.connection(id)
    local dbs = {}
    for _, d in ipairs(c and c.databases or {}) do
      if opts.deep ~= "listed" or d.listed then
        dbs[#dbs + 1] = d.name
      end
    end
    local left, first_err = #dbs, nil
    if left == 0 then
      return cb and cb(nil)
    end
    for _, name in ipairs(dbs) do
      M.database(id, name, opts, function(e)
        first_err = first_err or e
        left = left - 1
        if left == 0 and cb then
          cb(first_err)
        end
      end)
    end
  end
  once(M.conn_key(id), deep, function(done)
    request("databases.list", { connection = id }, function(err, res)
      c = manifest.connection(id)
      if not c then
        return done(err or "connection " .. id .. " is gone")
      end
      if err then
        c.error = message(err)
        pcall(manifest.save)
        return done(err, "can't list the databases of " .. c.name .. ": " .. message(err) .. " " .. M.retry_hint(c))
      end
      apply(done, function()
        local list = {}
        for _, name in ipairs(res.databases) do
          list[#list + 1] = manifest.database(c, name) or { name = name, schemas = {} }
        end
        c.databases = list
        place_databases(c)
        c.listed, c.error = os.time(), nil
        manifest.save()
      end)
    end)
  end)
end

--- Whether nothing is being listed.
function M.idle()
  return next(M.busy) == nil
end

return M
