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
local sync = require("dbear.tree.sync")

local M = {}

local ns = vim.api.nvim_create_namespace("dbear.tree.status")
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

--- The status line for a folder: listing, or the last listing's error.
local function status(path)
  local rel = fs.rel(path)
  if rel == "" then
    if sync.busy.root then
      return "listing connections…", "DbearSummary"
    end
    local err = manifest.get().error
    return err and ("✗ " .. err .. " (:DbearSync retries)"), "DbearError"
  end
  local hit = rel and manifest.lookup(rel)
  if not hit then
    return sync.busy.root and "listing connections…" or nil, "DbearSummary"
  end
  local c, d = hit.conn, hit.db
  if sync.busy[sync.conn_key(c.id)] then
    return (c.browses and "listing databases of " or "listing ") .. c.name .. "…", "DbearSummary"
  end
  if d and sync.busy[sync.db_key(c.id, d.name)] then
    return "listing " .. d.name .. "…", "DbearSummary"
  end
  local err = (d and d.error) or c.error
  if err then
    return "✗ " .. err .. " (:DbearSync " .. c.name .. " retries)", "DbearError"
  end
end

local function show_status(buf)
  local path = folder_of(buf)
  if not path or not vim.api.nvim_buf_is_loaded(buf) then
    return
  end
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  local text, hl = status(path)
  vim.b[buf].dbear_tree_status = text
  if text then
    -- A line of its own under the first one (`../` in oil): long errors stay readable.
    pcall(vim.api.nvim_buf_set_extmark, buf, ns, 0, 0, { virt_lines = { { { "dbear: " .. text, hl } } } })
  end
end

--- Shows a folder buffer's new contents: oil re-reads it, other buffers are `:edit`ed.
local function reload(buf, is_oil)
  if vim.bo[buf].modified then
    return
  end
  local wins = vim.fn.win_findbuf(buf)
  if is_oil then
    local ok, view = pcall(require, "oil.view")
    if ok and view.render_buffer_async then
      if #wins > 0 then
        pcall(view.render_buffer_async, buf, {}, function() show_status(buf) end)
      else
        vim.b[buf].oil_dirty = {}
      end
    end
  elseif #wins > 0 and vim.bo[buf].filetype == "netrw" then
    pcall(vim.api.nvim_win_call, wins[1], function() vim.cmd("silent! edit") end)
  end
end

local timer
local function refresh_all(reread)
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    local path, is_oil = folder_of(buf)
    if path then
      if reread then
        reload(buf, is_oil)
      end
      show_status(buf)
    end
  end
  -- While something is listed, keep the statuses up (folder plugins redraw their buffers).
  if not sync.idle() and not timer then
    timer = vim.uv.new_timer()
    timer:start(300, 300, vim.schedule_wrap(function()
      refresh_all(false)
      if sync.idle() and timer then
        timer:stop()
        timer:close()
        timer = nil
      end
    end))
  end
end

sync.on_change = function(changed)
  vim.schedule(function() refresh_all(changed) end)
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
        callback = function() pcall(fs.unlock, file, false) end,
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
      local path = folder_of(ev.buf)
      if path then
        M.entered(path)
        show_status(ev.buf)
      end
    end,
  })
end

return M
