--- dbear for Neovim: runs SQL on dbear's saved connections through `dbear-cli serve`.
--- Each buffer has a connection (`b:dbear_connection`, an id, name or URL) and optionally a
--- database (`b:dbear_database`); `g:dbear_connection` is the fallback.

local render = require("dbear.render")

local M = {}

M.config = {
  cmd = { "dbear-cli", "serve" }, -- or set `bin` to the binary's path
  bin = nil,
  env = nil, -- extra environment for the server
  max_rows = 1000, -- rows kept per result (0: all)
  record_history = true, -- add runs to the connection's history (shared with the apps)
  null = "NULL", -- how NULL looks (highlighted DbearNull, so it differs from the text "NULL")
  max_width = 60, -- cut cells wider than this (display cells; 0: never)
  split = "botright 15split", -- how the results window opens
  browse = {
    page_size = 100, -- rows per page in table buffers
    max_width = 40, -- longer values are cut (…) in table buffers; edit them with <CR>
  },
  tree = { -- the mirror: connections as folders, tables as files (see :h dbear / README)
    enabled = true, -- false: no mirror; :DbearOpen opens the dbear:// list
    root = nil, -- default: stdpath("cache") .. "/dbear/tree"
    relist = "session", -- list a folder again the first time it's entered in a session; "never": only once
  },
}

local client
local running = {} -- buffer → { [request id] = true }

local function notify(msg, level)
  vim.notify("dbear: " .. msg, level or vim.log.levels.INFO)
end

local function rpc()
  if not client then
    local cmd = M.config.bin and { vim.fn.expand(M.config.bin), "serve" } or M.config.cmd
    client = require("dbear.rpc").new({
      cmd = cmd,
      env = M.config.env,
      on_exit = function(code, stderr)
        if code ~= 0 then
          notify("dbear-cli exited (" .. code .. ")" .. (stderr ~= "" and ": " .. stderr or ""), vim.log.levels.WARN)
        end
      end,
    })
  end
  return client
end
M.rpc = rpc

function M.setup(opts)
  M.config = vim.tbl_deep_extend("force", M.config, opts or {})
  render.opts.null, render.opts.max_width = M.config.null, M.config.max_width
  require("dbear.tree").attach()
  if client then -- the next request starts the server with the new options
    client:stop()
    client = nil
  end
end

--- Calls `method`; `cb(result)` runs only on success, errors are shown.
local function call(method, params, cb)
  return rpc():request(method, params, function(err, result)
    if err then
      notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    else
      cb(result)
    end
  end)
end

-- MARK: Buffer target

local function target(buf)
  local b = vim.b[buf or 0]
  local conn = b.dbear_connection or vim.g.dbear_connection
  return conn and { connection = conn, database = b.dbear_database } or nil
end
M.target = target

local function set_connection(buf, c, spec)
  -- A URL's id ("url:…") only lives as long as the server, so keep the URL itself.
  vim.b[buf].dbear_connection = vim.startswith(c.id, "url:") and spec or c.id
  vim.b[buf].dbear_connection_name = c.name
  vim.b[buf].dbear_database = nil
  vim.bo[buf].omnifunc = "v:lua.require'dbear'.omnifunc"
end

--- `:Dbear [name]`: sets the buffer's connection, or picks one.
function M.connect(spec, cb)
  local buf = vim.api.nvim_get_current_buf()
  if spec and spec ~= "" then
    return call("connections.get", { connection = spec }, function(c)
      set_connection(buf, c, spec)
      notify("using " .. c.name)
      if cb then cb() end
    end)
  end
  call("connections.list", nil, function(res)
    if #res.connections == 0 then
      return notify("no saved connections", vim.log.levels.WARN)
    end
    vim.ui.select(res.connections, {
      prompt = "dbear connection",
      format_item = function(c) return c.name .. "  " .. c.summary end,
    }, function(c)
      if c then
        set_connection(buf, c)
        if cb then cb() end
      end
    end)
  end)
end

--- Runs `fn(target)` with the buffer's target, asking for a connection first when it has none.
local function with_target(fn)
  local t = target()
  if t then
    return fn(t)
  end
  M.connect(nil, function() fn(target()) end)
end

function M.connection_names()
  local err, res = rpc():request_sync("connections.list", nil, 2000)
  local names = {}
  for _, c in ipairs(not err and res and res.connections or {}) do
    names[#names + 1] = c.name
  end
  return names
end

--- `:DbearDatabase [name]`: another database on the connection's server.
function M.database(name)
  with_target(function(t)
    if name and name ~= "" then
      vim.b.dbear_database = name
      return
    end
    local buf = vim.api.nvim_get_current_buf()
    call("databases.list", { connection = t.connection }, function(res)
      vim.ui.select(res.databases, {
        prompt = "dbear database",
        format_item = function(d) return d == res.current and d .. " (current)" or d end,
      }, function(d)
        if d then
          vim.b[buf].dbear_database = d ~= res.default and d or nil
        end
      end)
    end)
  end)
end

-- MARK: Results window

local ns = vim.api.nvim_create_namespace("dbear")

local function results_buffer()
  local buf = vim.fn.bufnr("dbear://results")
  if buf == -1 or not vim.api.nvim_buf_is_valid(buf) then
    buf = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_buf_set_name(buf, "dbear://results")
    vim.bo[buf].filetype = "dbear"
  end
  return buf
end

--- Shows `lines` in the results split (opened when hidden), keeping the cursor where it is.
function M.show(lines, hls)
  local buf = results_buffer()
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  for _, h in ipairs(hls or {}) do
    local line = lines[h[1] + 1] or ""
    vim.api.nvim_buf_set_extmark(buf, ns, h[1], h[2], { end_col = h[3] < 0 and #line or h[3], hl_group = h[4] })
  end
  local win = vim.fn.bufwinid(buf)
  if win == -1 then
    local from = vim.api.nvim_get_current_win()
    vim.cmd(M.config.split)
    win = vim.api.nvim_get_current_win()
    vim.api.nvim_win_set_buf(win, buf)
    vim.wo[win].wrap, vim.wo[win].number, vim.wo[win].list = false, false, false
    vim.api.nvim_set_current_win(from)
  end
  vim.api.nvim_win_set_cursor(win, { 1, 0 })
  return buf
end

-- MARK: Commands

--- The SQL for `:DbearRun`: the visual selection, the range's lines, or the whole buffer.
local function selected_sql(opts)
  if opts.range == 0 then
    return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")
  end
  local vs, ve = vim.fn.getpos("'<"), vim.fn.getpos("'>")
  if vs[2] == opts.line1 and ve[2] == opts.line2 and vim.fn.visualmode() ~= "" then
    return table.concat(vim.fn.getregion(vs, ve, { type = vim.fn.visualmode() }), "\n")
  end
  return table.concat(vim.api.nvim_buf_get_lines(0, opts.line1 - 1, opts.line2, false), "\n")
end

--- `:DbearRun`. `opts` are the command's (`range`, `line1`, `line2`), or `{ sql = … }`.
function M.run(opts)
  opts = opts or { range = 0 }
  local sql = opts.sql or selected_sql(opts)
  if vim.trim(sql) == "" then
    return notify("nothing to run", vim.log.levels.WARN)
  end
  with_target(function(t)
    local buf = vim.api.nvim_get_current_buf()
    M.show({ "Running… (:DbearCancel stops it)" })
    running[buf] = running[buf] or {}
    local id
    id = rpc():request("query.run", {
      connection = t.connection,
      database = t.database,
      sql = sql,
      max_rows = M.config.max_rows,
      record_history = M.config.record_history,
    }, function(err, res)
      if id and running[buf] then
        running[buf][id] = nil
      end
      if err then
        M.show({ "Error: " .. (err.message or "?") }, { { 0, 0, -1, "DbearError" } })
      else
        M.show(render.outcome(res))
      end
    end)
    if id then
      running[buf][id] = true
    end
  end)
end

--- `:DbearCancel`: stops the queries this buffer started.
function M.cancel()
  local ids = vim.tbl_keys(running[vim.api.nvim_get_current_buf()] or {})
  if #ids == 0 then
    return notify("nothing running")
  end
  for _, id in ipairs(ids) do
    rpc():request("query.cancel", { id = id }, function() end)
  end
end

--- `:DbearTables`
function M.tables(refresh)
  with_target(function(t)
    call("schemas.list", { connection = t.connection, database = t.database, refresh = refresh or false }, function(res)
      M.show(render.tables(res))
    end)
  end)
end

--- `:DbearDescribe [table]`: the word under the cursor when no table is given.
function M.describe(name)
  name = (name and name ~= "") and name or vim.fn.expand("<cword>")
  if name == "" then
    return notify("which table?", vim.log.levels.WARN)
  end
  with_target(function(t)
    call("table.describe", { connection = t.connection, database = t.database, table = name }, function(res)
      M.show(render.describe(res))
    end)
  end)
end

--- `:DbearHistory`: pick a past query and insert it below the cursor.
function M.history()
  with_target(function(t)
    local win = vim.api.nvim_get_current_win()
    call("history.list", { connection = t.connection, limit = 100 }, function(res)
      if #res.entries == 0 then
        return notify("no history yet")
      end
      vim.ui.select(res.entries, {
        prompt = "dbear history",
        format_item = function(e)
          local sql = e.sql:gsub("%s+", " ")
          return os.date("%Y-%m-%d %H:%M", math.floor(e.ran_at / 1000)) .. (e.error ~= vim.NIL and e.error and "  ✗ " or "  ") .. sql
        end,
      }, function(e)
        if e and vim.api.nvim_win_is_valid(win) then
          local row = vim.api.nvim_win_get_cursor(win)[1]
          vim.api.nvim_buf_set_lines(vim.api.nvim_win_get_buf(win), row, row, false, vim.split(e.sql, "\n", { plain = true }))
        end
      end)
    end)
  end)
end

-- MARK: Completion

local completion = { items = {} }

--- 'omnifunc' from the server's `complete`. Offsets are UTF-8 bytes on both sides.
function M.omnifunc(findstart)
  if findstart ~= 1 then -- `base` is what the server already matched against
    return completion.items
  end
  completion.items = {}
  local t = target()
  if not t then
    return -3
  end
  local row, col = unpack(vim.api.nvim_win_get_cursor(0))
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  local line_start = 0
  for i = 1, row - 1 do
    line_start = line_start + #lines[i] + 1
  end
  local err, res = rpc():request_sync("complete", {
    connection = t.connection,
    database = t.database,
    sql = table.concat(lines, "\n"),
    offset = line_start + col,
  }, 3000)
  if err or not res then
    return -3
  end
  local kinds = { keyword = "k", schema = "s", table = "t", view = "v", column = "c", ["function"] = "f" }
  for _, item in ipairs(res.items) do
    completion.items[#completion.items + 1] = {
      word = item.insert_text,
      abbr = item.label,
      menu = item.detail ~= vim.NIL and item.detail or nil,
      kind = kinds[item.kind] or item.kind,
    }
  end
  local start = res.replace_start - line_start
  return (start >= 0 and start <= col) and start or col
end

return M
