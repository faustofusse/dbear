--- The mirror: the saved connections as real folders and files, so oil.nvim, netrw, telescope or
--- anything else that works on paths can browse them.
---
---   <root>/<connection>/<database>/<schema>/<table>.table   (and <view>.view)
---
--- Always four levels. Database folders are what `databases.list` returns for connections that
--- browse their server's databases; other connections have one, their own database (for SQLite the
--- file's name). Schema folders are `schemas.list`'s (`main` for SQLite, the database again for
--- MySQL, which has no schemas below databases). A table file holds a header line with its exact
--- target and its column list; opening it shows the editable rows buffer (`browse.grid`) instead.
---
--- Nothing is listed at startup. Entering the root lists the connections (from the store, no
--- network); entering a connection folder lists its databases, or its own database's tables;
--- entering a database or schema folder lists its tables, once per session (`tree.relist`).
--- `:DbearSync` lists again. See `tree.sync` for the listing, `tree.fs` for the guarded file system.

local fs = require("dbear.tree.fs")
local manifest = require("dbear.tree.manifest")
local progress = require("dbear.tree.progress")
local sync = require("dbear.tree.sync")

local M = {}

local group = vim.api.nvim_create_augroup("dbear.tree", { clear = true })
local files_group = vim.api.nvim_create_augroup("dbear.tree.files", { clear = true })

local function dbear()
  return require("dbear")
end

local function config()
  return dbear().config.tree
end

local function notify(msg, level)
  vim.notify("dbear: " .. msg, level or vim.log.levels.INFO)
end

function M.enabled()
  local c = config()
  return c ~= false and c.enabled ~= false
end

function M.root()
  return fs.root()
end

-- MARK: Folder buffers

--- The folder a buffer shows (oil, netrw or a plain directory buffer), when it's in the mirror.
local function folder_of(buf)
  local name = vim.api.nvim_buf_get_name(buf)
  local oil = name:match("^oil://(.*)$")
  local path = fs.inside(oil or name)
  if not path then
    return nil
  end
  if oil or vim.fn.isdirectory(path) == 1 then
    return path, oil ~= nil
  end
end
M.folder_of = folder_of

--- The listing a folder shows: its key, the connection (nil at the root), the last listing's
--- error and the key of the listing that failed. A database or schema folder of a connection that
--- browses databases is its database's listing (its error, else its connection's databases'); any
--- other folder below the root is its connection's.
local function listing_of(path)
  local rel = fs.rel(path)
  if rel == "" then
    return "root", nil, manifest.exists() and manifest.get().error or nil, "root"
  end
  local hit = rel and manifest.exists() and manifest.lookup(rel)
  if not hit then
    return nil
  end
  local c, d = hit.conn, hit.db
  local ck = sync.conn_key(c.id)
  if hit.level ~= "conn" and c.browses and d then
    local dk = sync.db_key(c.id, d.name)
    return dk, c, d.error or c.error, d.error and dk or ck
  end
  return ck, c, c.error, ck
end

--- Failures per listing key in this session, and the failure each key's warning was given for.
local failures, warned = {}, {}

--- What a listing is about, for its progress: "connections", "databases of Staging", "Staging / app".
local function label(key)
  if key == "root" then
    return "connections"
  end
  local id, db = key:match("^d:(.-)%z(.*)$")
  id = id or key:match("^c:(.*)$")
  local c = id and manifest.connection(id)
  local name = c and c.name or id or key
  if db then
    return name .. " / " .. db
  end
  return (c and c.browses and "databases of " or "") .. name
end

sync.on_start = function(key)
  progress.start(key, label(key))
end

sync.on_finish = function(key, err)
  progress.finish(key, err)
  if err then
    failures[key] = (failures[key] or 0) + 1
  end
end

--- Coming back to a folder whose last listing failed (and that isn't being listed again): one
--- warning per failure, with how to retry. The failure itself was an error message; "coming
--- back" is entering the folder in a window that showed something else, so focusing a window,
--- `:edit` or a redraw of the same folder says nothing. A failure recorded in an earlier session
--- (in the manifest) warns once too.
local function warn_failed(win, path)
  local was = vim.w[win].dbear_tree_folder
  vim.w[win].dbear_tree_folder = path
  if was == path then
    return
  end
  local key, c, err, failed = listing_of(path)
  if not key or not err or sync.busy[key] or sync.busy[failed] then
    return
  end
  local n = failures[failed] or 0
  if warned[failed] == n then
    return
  end
  warned[failed] = n
  notify("the last listing of " .. (c and label(failed) or "the connections") .. " failed: " .. err .. " "
    .. sync.retry_hint(c), vim.log.levels.WARN)
end

--- Shows a folder buffer's new contents: oil re-reads it, other buffers are `:edit`ed.
local function reload(buf, is_oil)
  if vim.bo[buf].modified then
    return
  end
  local wins = vim.fn.win_findbuf(buf)
  if is_oil then
    local busy, mutator = pcall(require, "oil.mutator")
    if busy and mutator.is_mutating() then
      return -- oil redraws its buffers itself when it's done
    end
    local ok, view = pcall(require, "oil.view")
    if ok and view.render_buffer_async then
      if #wins > 0 then
        pcall(view.render_buffer_async, buf, {})
      else
        vim.b[buf].oil_dirty = {}
      end
    end
  elseif #wins > 0 and vim.bo[buf].filetype == "netrw" then
    pcall(vim.api.nvim_win_call, wins[1], function() vim.cmd("silent! edit") end)
  end
end

local function refresh_all()
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    local path, is_oil = folder_of(buf)
    if path then
      reload(buf, is_oil)
    end
  end
end

sync.on_change = function(changed)
  if changed then
    vim.schedule(refresh_all)
  end
end

-- MARK: Renamed folders

--- `name` (a buffer name: a path, maybe `oil://`, under the root or its realpath) with folder
--- `from` (a path under the configured root) replaced by `to`, or nil when it isn't below `from`.
local function swap(name, from, to)
  local rel_from, rel_to = fs.rel(from), fs.rel(to)
  for _, base in ipairs({ fs.root(), fs.real_root() }) do
    for _, prefix in ipairs({ "", "oil://" }) do
      local head = prefix .. base .. "/" .. rel_from
      local rest = name:sub(#head + 1)
      if name:sub(1, #head) == head and (rest == "" or rest:sub(1, 1) == "/") then
        return prefix .. base .. "/" .. rel_to .. rest
      end
    end
  end
end

--- A connection folder moved from `from` to `to` (both under the configured root): buffers below
--- it follow. Rows buffers (table files) are renamed, keeping their rows and pending edits, and
--- their `-` goes to the new folder; hidden folder buffers (oil) are dropped, shown ones renamed.
function M.moved(from, to)
  fs.moved(from, to)
  local grid = package.loaded["dbear.browse.grid"]
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    local name = vim.api.nvim_buf_get_name(buf)
    local new = name ~= "" and swap(name, from, to)
    if new then
      local is_oil = vim.startswith(name, "oil://")
      if is_oil and #vim.fn.win_findbuf(buf) == 0 and not vim.bo[buf].modified then
        pcall(vim.api.nvim_buf_delete, buf, { force = true })
      else
        local ok = pcall(vim.api.nvim_buf_set_name, buf, new)
        if ok then
          -- Renaming leaves an unlisted, unloaded buffer with the old name behind.
          local stale = vim.fn.bufnr("^" .. vim.fn.escape(name, "\\^$.*~[]") .. "$")
          if stale > 0 and stale ~= buf and not vim.api.nvim_buf_is_loaded(stale) then
            pcall(vim.api.nvim_buf_delete, stale, { force = true })
          end
          local file = vim.b[buf].dbear_tree_file
          if file then
            vim.b[buf].dbear_tree_file = swap(file, from, to) or file
          end
          local st = grid and grid.states[buf]
          if st and st.spec.dir then
            st.spec.dir = swap(st.spec.dir, from, to) or st.spec.dir
          end
          if is_oil then
            reload(buf, true)
          end
        end
      end
    end
  end
end

sync.on_moved = function(from, to)
  M.moved(from, to)
end

-- MARK: oil

--- oil makes the buffer of a folder it can't write nomodifiable. The root stays read-only (r-x) on
--- disk, so nothing can be created, deleted or copied there, but its oil buffer is made
--- modifiable so a connection folder's name can be edited: `OilActionsPre` then lets exactly
--- those renames through (see `oil_pre`). Done through oil's adapter interface (`is_modifiable`).
local patched_oil = false
local function patch_oil()
  if patched_oil or config().rename == false then
    return
  end
  local ok, files = pcall(require, "oil.adapters.files")
  if not ok or type(files) ~= "table" or type(files.is_modifiable) ~= "function" then
    return
  end
  patched_oil = true
  local original = files.is_modifiable
  files.is_modifiable = function(bufnr)
    local oil = vim.api.nvim_buf_get_name(bufnr):match("^oil://(.*)$")
    if oil and M.enabled() and config().rename ~= false and fs.rel(oil) == "" and manifest.exists() then
      return true
    end
    return original(bufnr)
  end
end

local function root_buffer_modifiable(buf)
  local oil = vim.api.nvim_buf_get_name(buf):match("^oil://(.*)$")
  if not (oil and patched_oil and fs.rel(oil) == "" and manifest.exists()) then
    return
  end
  local ok, mutator = pcall(require, "oil.mutator")
  if not vim.b[buf].oil_rendering and not (ok and mutator.is_mutating()) then
    vim.bo[buf].modifiable = true
  end
end

local function oil_path(url)
  local p = type(url) == "string" and url:match("^oil://(.*)$")
  return p and fs.inside(p) or nil
end

--- The renames being let through: `{ { src, dest } }`, with a timer that ends them anyway.
local granted

local function revoke()
  if not granted then
    return
  end
  local g = granted
  granted = nil
  if g.timer then
    pcall(function() g.timer:stop() g.timer:close() end)
  end
  pcall(fs.chmod, fs.root(), fs.DIR_MODE)
  for _, pair in ipairs(g) do
    for _, path in ipairs(pair) do
      if fs.kind(path) == "directory" then
        pcall(fs.chmod, path, fs.DIR_MODE)
      end
    end
  end
end

--- `User OilActionsPre`: when every action of the batch that touches the mirror renames a
--- connection folder in place (at the root, to a name nothing has), the root and those folders
--- are made writable until `OilActionsPost`, so the renames succeed; anything else in the mirror
--- (delete, create, copy, a move into a folder, a change below the root) keeps failing on the
--- read-only folders, and nothing at all is let through for that batch.
function M.oil_pre(actions)
  revoke()
  local touched, pairs_ok, refused = false, {}, false
  for _, a in ipairs(actions or {}) do
    local src, dest = oil_path(a.src_url), oil_path(a.dest_url)
    local url = oil_path(a.url)
    if src or dest or url then
      touched = true
      local rs, rd = src and fs.rel(src), dest and fs.rel(dest)
      local hit = rs and rs ~= "" and not rs:find("/", 1, true) and manifest.lookup(rs)
      local ok = a.type == "move" and a.entry_type == "directory" and hit and hit.level == "conn"
        and rd and rd ~= "" and not rd:find("/", 1, true)
      -- Never onto something that exists (rename would replace an empty folder); a case-only
      -- rename on a case-insensitive file system is the same folder.
      if ok and fs.kind(dest) and fs.identity(dest) ~= fs.identity(src) then
        ok = false
      end
      if ok then
        pairs_ok[#pairs_ok + 1] = { src, dest }
      else
        refused = true
      end
    end
  end
  if not touched then
    return
  end
  M.oil_batch = true
  if refused or #pairs_ok == 0 then
    notify("the mirror is read-only: only renaming a connection folder (at the root) is possible", vim.log.levels.WARN)
    return
  end
  granted = pairs_ok
  local ok, err = pcall(function()
    fs.chmod(fs.root(), fs.WRITABLE_DIR)
    for _, pair in ipairs(pairs_ok) do
      fs.chmod(pair[1], fs.WRITABLE_DIR)
    end
  end)
  if not ok then
    revoke()
    return notify(tostring(err), vim.log.levels.ERROR)
  end
  -- oil always sends OilActionsPost; this is in case it doesn't.
  granted.timer = vim.defer_fn(revoke, 30000)
end

--- `User OilActionsPost`: the mirror is read-only again, and the root is listed: renamed folders
--- are renamed in the store, anything else is undone.
function M.oil_post()
  revoke()
  if not M.oil_batch then
    return
  end
  M.oil_batch = nil
  -- After oil is done (it redraws its buffers then): renames go to the store, the rest is undone.
  local ok, mutator = pcall(require, "oil.mutator")
  local tries = 0
  local function later()
    tries = tries + 1
    if ok and mutator.is_mutating() and tries < 100 then
      return vim.defer_fn(later, 50)
    end
    sync.root_again()
  end
  vim.schedule(later)
end

-- MARK: :DbearRename

--- The saved connection the current buffer is about: a mirror folder (at the root, the entry
--- under oil's cursor), a table file or rows buffer, or a SQL buffer's `b:dbear_connection`.
local function current_connection(buf)
  local path, is_oil = folder_of(buf)
  local rel = path and fs.rel(path)
  if rel == "" and is_oil then
    local ok, oil = pcall(require, "oil")
    local entry = ok and oil.get_cursor_entry()
    rel = entry and entry.name or nil
  end
  local hit = rel and rel ~= "" and manifest.exists() and manifest.lookup(vim.split(rel, "/", { plain = true })[1])
  if hit and hit.conn then
    return hit.conn.id
  end
  local grid = package.loaded["dbear.browse.grid"]
  local st = grid and grid.states[buf]
  if st and st.spec.conn then
    return st.spec.conn
  end
  return vim.b[buf].dbear_connection
end

--- `:DbearRename [name]`: renames the current connection (see `current_connection`) in the store,
--- then its mirror folder follows. Without a name, asks for one.
function M.rename(name)
  local spec = current_connection(vim.api.nvim_get_current_buf())
  if not spec then
    return notify("no connection here: use it in a mirror folder, a table buffer or a SQL buffer with a connection", vim.log.levels.WARN)
  end
  dbear().rpc():request("connections.get", { connection = spec }, function(err, res)
    if err then
      return notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    end
    if vim.startswith(res.id, "url:") then
      return notify("connections given as URLs aren't saved, so they can't be renamed", vim.log.levels.WARN)
    end
    local function go(new)
      if not new then
        return
      end
      sync.rename(res.id, res.name, new, function(e)
        if not e and M.enabled() and manifest.exists() then
          sync.root_again()
        end
      end)
    end
    if name and name ~= "" then
      return go(name)
    end
    vim.ui.input({ prompt = "Rename “" .. res.name .. "” to: ", default = res.name }, go)
  end)
end

-- MARK: Lazy listing

local last_root = 0

local function listed(key, entry)
  if sync.listed[key] then
    return true
  end
  return config().relist == "never" and entry and entry.listed ~= nil
end

--- A folder of the mirror was entered: lists what it needs, once per session.
function M.entered(path)
  local rel = fs.rel(path)
  if not rel then
    return
  end
  if rel == "" or not manifest.exists() then
    if vim.uv.now() - last_root > 2000 then
      last_root = vim.uv.now()
      local ok, err = pcall(fs.ensure_root)
      if not ok then
        return notify(tostring(err), vim.log.levels.ERROR)
      end
      sync.root()
    end
    return
  end
  local hit = manifest.lookup(rel)
  if not hit then
    return sync.root()
  end
  local c = hit.conn
  if hit.level == "conn" or not c.browses then
    if not listed(sync.conn_key(c.id), c) then
      sync.connection(c.id)
    end
  elseif hit.db and not listed(sync.db_key(c.id, hit.db.name), hit.db) then
    sync.database(c.id, hit.db.name)
  end
end

-- MARK: Table files

--- BufReadCmd for a table file: the rows buffer of the target in its header.
function M.read_file(buf, path)
  local text = fs.read(path) or ""
  local h = sync.parse_header(text:match("^[^\n]*"))
  if not h then
    vim.bo[buf].buftype = "nofile"
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "Error: not a dbear mirror table file (no header): " .. path })
    vim.bo[buf].modified = false
    return
  end
  require("dbear.browse.grid").open(buf, {
    kind = "table",
    conn = h.connection,
    db = h.browses and h.database or nil,
    schema = h.schema,
    table = h.table,
    mirror = true,
    dir = vim.fs.dirname(vim.api.nvim_buf_get_name(buf)),
  })
  -- `:w` saves rows (BufWriteCmd), never the file, but Vim refuses it for a file it can't write
  -- (E505): the file is writable while its buffer is loaded (its folder stays read-only).
  local file = fs.inside(path)
  if file and pcall(fs.unlock, file, true) then
    vim.bo[buf].readonly = false
    if not vim.b[buf].dbear_tree_file then
      vim.b[buf].dbear_tree_file = file
      vim.api.nvim_create_autocmd("BufUnload", {
        group = files_group,
        buffer = buf,
        once = true,
        -- The file may have moved with its connection's folder since (see `M.moved`).
        callback = function(ev) pcall(fs.unlock, vim.b[ev.buf].dbear_tree_file or file, false) end,
      })
    end
  end
end

--- The mirror folder of a table's schema (for `-` in rows buffers reached from the mirror).
function M.dir_of(spec)
  if not M.enabled() or not manifest.exists() then
    return nil
  end
  local c = manifest.connection(spec.conn)
  if not c then
    for _, x in ipairs(manifest.get().connections) do
      if x.name == spec.conn then
        c = x
      end
    end
  end
  if not c then
    return nil
  end
  local d = spec.db and manifest.database(c, spec.db) or (not c.browses and c.databases[1]) or nil
  if not d then
    return manifest.path(c)
  end
  for _, s in ipairs(d.schemas) do
    if s.name == spec.schema then
      return manifest.path(c, d, s)
    end
  end
  return manifest.path(c, d)
end

-- MARK: Commands

--- Opens the mirror root (or a connection's folder) in the current window.
function M.open(conn)
  local ok, err = pcall(fs.ensure_root)
  if not ok then
    return notify(tostring(err), vim.log.levels.ERROR)
  end
  if not conn or conn == "" then
    return vim.cmd.edit(vim.fn.fnameescape(fs.root()))
  end
  M.resolve(conn, function(id)
    local c = manifest.connection(id)
    if c then
      vim.cmd.edit(vim.fn.fnameescape(manifest.path(c)))
    end
  end)
end

--- `cb(id)` for a saved connection's name or id (after listing the connections), else a message.
function M.resolve(spec, cb)
  if spec:match("^%a[%w+.-]*:") then
    return notify("connections given as URLs aren't mirrored: save it in dbear first", vim.log.levels.WARN)
  end
  dbear().rpc():request("connections.get", { connection = spec }, function(err, res)
    if err then
      return notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    end
    if vim.startswith(res.id, "url:") then
      return notify("connections given as URLs aren't mirrored", vim.log.levels.WARN)
    end
    sync.root(nil, function(e)
      if not e then
        cb(res.id)
      end
    end)
  end)
end

--- `:DbearSync[!] [connection]`. Without a connection: the connection folders, and every
--- connection and database listed before. With one: its databases and all their tables. `!`
--- rebuilds what it lists (rewrites every file, recreates schema folders, resets permissions).
--- `done(err)` when it's all over.
function M.sync(arg, bang, done)
  done = done or function() end
  local ok, err = pcall(fs.ensure_root)
  if not ok then
    notify(tostring(err), vim.log.levels.ERROR)
    return done(err)
  end
  local opts = { fresh = bang or false }
  local function report(e, what)
    if not e then
      notify("synced " .. what)
    end
    done(e)
  end
  arg = vim.trim(arg or "")
  if arg ~= "" then
    return M.resolve(arg, function(id)
      opts.deep = true
      sync.connection(id, opts, function(e) report(e, manifest.connection(id).name) end)
    end)
  end
  sync.root(opts, function(e)
    if e then
      return done(e)
    end
    local todo = {}
    for _, c in ipairs(manifest.get().connections) do
      local any = c.listed ~= nil
      for _, d in ipairs(c.databases) do
        any = any or d.listed ~= nil
      end
      if any then
        todo[#todo + 1] = c.id
      end
    end
    local left, first = #todo, nil
    if left == 0 then
      return report(nil, "the connections")
    end
    opts.deep = "listed"
    for _, id in ipairs(todo) do
      sync.connection(id, opts, function(x)
        first = first or x
        left = left - 1
        if left == 0 then
          report(first, #todo .. " connection" .. (#todo == 1 and "" or "s"))
        end
      end)
    end
  end)
end

--- The table files in the mirror, relative to the root, from the manifest.
function M.files()
  local out = {}
  if not manifest.exists() then
    return out
  end
  for _, c in ipairs(manifest.get().connections) do
    for _, d in ipairs(c.databases) do
      for _, s in ipairs(d.schemas) do
        for _, t in ipairs(s.tables) do
          out[#out + 1] = table.concat({ c.dir, d.dir, s.dir, t.file }, "/")
        end
      end
    end
  end
  return out
end

local function telescope()
  local ok, builtin = pcall(require, "telescope.builtin")
  return ok and builtin or nil
end

--- `:DbearFind`: pick a table (telescope's find_files on the root, else vim.ui.select).
function M.find()
  if not manifest.exists() then
    return notify("nothing in the mirror yet: browse it (:DbearOpen) or :DbearSync", vim.log.levels.WARN)
  end
  local builtin = telescope()
  if builtin then
    return builtin.find_files({ cwd = fs.root(), prompt_title = "dbear tables" })
  end
  vim.ui.select(M.files(), { prompt = "dbear table" }, function(file)
    if file then
      vim.cmd.edit(vim.fn.fnameescape(fs.root() .. "/" .. file))
    end
  end)
end

--- `:DbearGrep [pattern]`: search the column lists (telescope's live_grep on the root, else
--- ripgrep or grep into the quickfix list).
function M.grep(pattern)
  if not manifest.exists() then
    return notify("nothing in the mirror yet: browse it (:DbearOpen) or :DbearSync", vim.log.levels.WARN)
  end
  local builtin = telescope()
  if builtin then
    return builtin.live_grep({ cwd = fs.root(), prompt_title = "dbear columns", default_text = pattern })
  end
  if not pattern or pattern == "" then
    return vim.ui.input({ prompt = "dbear grep: " }, function(p)
      if p and p ~= "" then
        M.grep(p)
      end
    end)
  end
  local cmd = vim.fn.executable("rg") == 1 and { "rg", "--vimgrep", "--no-heading", "-g", "*.table", "-g", "*.view", "--", pattern, "." }
    or { "grep", "-rnI", "--include=*.table", "--include=*.view", "--", pattern, "." }
  local out = vim.system(cmd, { cwd = fs.root(), text = true }):wait()
  local items = {}
  for line in (out.stdout or ""):gmatch("[^\n]+") do
    local file, lnum, text = line:match("^%./?([^:]+):(%d+):(.*)$")
    if file then
      text = text:gsub("^%d+:", "") -- rg's column
      items[#items + 1] = { filename = fs.root() .. "/" .. file, lnum = tonumber(lnum), text = text }
    end
  end
  vim.fn.setqflist({}, " ", { title = "dbear grep " .. pattern, items = items })
  if #items == 0 then
    return notify("no column matches " .. pattern)
  end
  vim.cmd("copen")
end

--- Whether nothing is being listed (tests wait on it).
function M.idle()
  return sync.idle()
end

-- MARK: Setup

local function pattern(path)
  return (path:gsub("[%s,%[%]{}*?\\]", "\\%0"))
end

--- (Re)registers the autocommands for the configured root. Touches nothing on disk.
function M.attach()
  vim.api.nvim_clear_autocmds({ group = group })
  if not M.enabled() then
    return
  end
  local root = config().root or (vim.fn.stdpath("cache") .. "/dbear/tree")
  if fs.root() ~= fs.normalize(vim.fn.fnamemodify(vim.fn.expand(root), ":p")) then
    fs.set_root(root)
    manifest.reset()
  end
  local roots = { fs.root() }
  if fs.real_root() and fs.real_root() ~= fs.root() then
    roots[2] = fs.real_root()
  end
  local patterns = {}
  for _, r in ipairs(roots) do
    patterns[#patterns + 1] = pattern(r) .. "/*.table"
    patterns[#patterns + 1] = pattern(r) .. "/*.view"
  end
  vim.api.nvim_create_autocmd("BufReadCmd", {
    group = group,
    pattern = patterns,
    callback = function(ev) M.read_file(ev.buf, ev.match) end,
  })
  vim.api.nvim_create_autocmd("BufEnter", {
    group = group,
    callback = function(ev)
      local path, is_oil = folder_of(ev.buf)
      if path then
        if is_oil then
          patch_oil()
          root_buffer_modifiable(ev.buf)
        end
        M.entered(path)
        warn_failed(vim.api.nvim_get_current_win(), path)
      elseif vim.w.dbear_tree_folder then
        vim.w.dbear_tree_folder = nil -- left the mirror's folders: entering one again is coming back
      end
    end,
  })
  -- oil: renaming connection folders at the root (see `patch_oil` and `oil_pre`).
  if config().rename ~= false then
    if package.loaded["oil"] then
      patch_oil()
    end
    vim.api.nvim_create_autocmd("User", {
      group = group,
      pattern = "OilEnter",
      callback = function(ev)
        local buf = type(ev.data) == "table" and ev.data.buf or ev.buf
        if buf and vim.api.nvim_buf_is_valid(buf) and folder_of(buf) then
          patch_oil()
          root_buffer_modifiable(buf)
        end
      end,
    })
    vim.api.nvim_create_autocmd("User", {
      group = group,
      pattern = "OilActionsPre",
      callback = function(ev) M.oil_pre(type(ev.data) == "table" and ev.data.actions or {}) end,
    })
    vim.api.nvim_create_autocmd("User", {
      group = group,
      pattern = "OilActionsPost",
      callback = function() M.oil_post() end,
    })
  end
end

return M
