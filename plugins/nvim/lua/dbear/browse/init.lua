--- The `dbear://` browser, oil.nvim style: buffers named `dbear://…` (see `browse.url`) list
--- connections, databases and tables, read-only, and a table's rows as an editable buffer
--- (`browse.grid`). `<CR>` enters, `-` goes up, all in the current window.

local url = require("dbear.browse.url")
local grid = require("dbear.browse.grid")
local ui = require("dbear.browse.ui")

local M = {}

local ns = vim.api.nvim_create_namespace("dbear.browse.list")

local function dbear()
  return require("dbear")
end

local function notify(msg, level)
  vim.notify("dbear: " .. msg, level or vim.log.levels.INFO)
end

local function open(spec)
  vim.cmd.edit(vim.fn.fnameescape(url.format(spec)))
end
M.open_spec = open

-- MARK: Lists (connections, databases, tables)

local lists = {} -- buffer → { spec, entries = { [lnum] = spec } }

--- Fills a list buffer: `items` are `{ text, spec? , hl? }`.
local function show_list(buf, items, title)
  if title then
    table.insert(items, 1, { title, nil, "DbearSummary" })
  end
  local lines, entries = {}, {}
  for i, item in ipairs(items) do
    lines[i] = item[1]
    entries[i] = item[2]
  end
  if #lines == 0 then
    lines = { "(empty)" }
  end
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.bo[buf].modified = false
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  for i, item in ipairs(items) do
    if item[3] then
      vim.api.nvim_buf_set_extmark(buf, ns, i - 1, 0, { end_col = #lines[i], hl_group = item[3] })
    end
    for _, h in ipairs(item[4] or {}) do
      vim.api.nvim_buf_set_extmark(buf, ns, i - 1, h[1], { end_col = h[2], hl_group = h[3] })
    end
  end
  lists[buf].entries = entries
  local win = vim.fn.bufwinid(buf)
  if win ~= -1 then
    for i = 1, #lines do
      if entries[i] then
        vim.api.nvim_win_set_cursor(win, { i, 0 })
        break
      end
    end
  end
end

local function fail(buf, err)
  show_list(buf, { { "Error: " .. (err.message or vim.inspect(err)), nil, "DbearError" } })
end

local function call(buf, method, params, fn)
  dbear().rpc():request(method, params, function(err, res)
    if not vim.api.nvim_buf_is_valid(buf) then
      return
    end
    if err then
      return fail(buf, err)
    end
    fn(res)
  end)
end

local function pad(s, w)
  return s .. string.rep(" ", w - vim.api.nvim_strwidth(s))
end

local function load_root(buf)
  call(buf, "connections.list", nil, function(res)
    local seen, w = {}, 0
    for _, c in ipairs(res.connections) do
      seen[c.name:lower()] = (seen[c.name:lower()] or 0) + 1
      w = math.max(w, vim.api.nvim_strwidth(c.name))
    end
    local items = {}
    for _, c in ipairs(res.connections) do
      -- A name that isn't unique can't find its connection: use the id.
      local spec = { kind = "conn", conn = seen[c.name:lower()] == 1 and c.name or c.id }
      local text = pad(c.name, w) .. "  " .. c.summary
      items[#items + 1] = { text, spec, nil, { { 0, #c.name, "DbearHeader" }, { #text - #c.summary, #text, "DbearSummary" } } }
    end
    show_list(buf, items, #items == 0 and "No saved connections: add some in the dbear app" or nil)
  end)
end

local function table_items(spec, res)
  local items, w = {}, 0
  for _, schema in ipairs(res.schemas) do
    for _, t in ipairs(schema.tables) do
      w = math.max(w, vim.api.nvim_strwidth(t.name))
    end
  end
  for _, schema in ipairs(res.schemas) do
    items[#items + 1] = { schema.name, nil, "DbearHeader" }
    for _, t in ipairs(schema.tables) do
      local n = t.estimated_row_count
      local rows = (n ~= nil and n ~= vim.NIL) and ("~" .. n) or ""
      local text = "  " .. pad(t.name, w) .. "  " .. pad(t.kind, 5) .. "  " .. rows
      local tspec = { kind = "table", conn = spec.conn, db = spec.db, schema = t.schema, table = t.name }
      items[#items + 1] = { text, tspec, nil, { { 2 + #t.name, #text, "DbearSummary" } } }
    end
  end
  return items
end

local function load_tables(buf, spec, refresh)
  call(buf, "schemas.list", { connection = spec.conn, database = spec.db, refresh = refresh or false }, function(res)
    show_list(buf, table_items(spec, res), (spec.db or res.target.database) .. " · tables")
  end)
end

local function load_conn(buf, spec, refresh)
  call(buf, "connections.get", { connection = spec.conn }, function(c)
    if not c.browses_databases then
      return load_tables(buf, spec, refresh)
    end
    call(buf, "databases.list", { connection = spec.conn }, function(res)
      local items = {}
      for _, d in ipairs(res.databases) do
        items[#items + 1] = { d .. (d == res.current and "  (current)" or ""), { kind = "db", conn = spec.conn, db = d } }
      end
      show_list(buf, items, c.name .. " · databases")
    end)
  end)
end

local list_keys = {
  { "<CR>", "Open the entry under the cursor" },
  { "-", "Go up" },
  { "<C-l>", "Reload" },
  { "g?", "This help" },
}

local function load_list(buf, refresh)
  local spec = lists[buf].spec
  if spec.kind == "root" then
    load_root(buf)
  elseif spec.kind == "conn" then
    load_conn(buf, spec, refresh)
  else
    load_tables(buf, spec, refresh)
  end
end

local function open_list(buf, spec)
  local first = lists[buf] == nil
  lists[buf] = { spec = spec, entries = {} }
  vim.bo[buf].buftype = "acwrite"
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = "dbear_list"
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "Loading…" })
  vim.bo[buf].modifiable = false
  vim.bo[buf].modified = false
  if first then
    local map = function(lhs, fn) vim.keymap.set("n", lhs, fn, { buffer = buf, nowait = true }) end
    map("<CR>", function()
      local target = lists[buf].entries[vim.api.nvim_win_get_cursor(0)[1]]
      if target then
        open(target)
      end
    end)
    map("-", function()
      if lists[buf].spec.kind ~= "root" then
        open(url.parent(lists[buf].spec))
      end
    end)
    map("<C-l>", function() load_list(buf, true) end)
    map("g?", function()
      local lines = {}
      for _, k in ipairs(list_keys) do
        lines[#lines + 1] = string.format("  %-6s %s", k[1], k[2])
      end
      ui.info("dbear", lines)
    end)
    vim.api.nvim_create_autocmd("BufWriteCmd", { buffer = buf, callback = function() notify("this list is read-only") end })
    vim.api.nvim_create_autocmd("BufWipeout", { buffer = buf, callback = function() lists[buf] = nil end })
  end
  load_list(buf, false)
end

-- MARK: Entry points

--- BufReadCmd for `dbear://*`.
function M.read(buf, name)
  local spec = url.parse(name)
  if not spec then
    return
  end
  if spec.kind == "table" then
    grid.open(buf, spec)
  else
    open_list(buf, spec)
  end
end

--- `:DbearOpen [conn[/db]/[schema.table]]` (or a full `dbear://` name, or a connection URL).
--- With the mirror on (and no `!`): no argument opens its root, a connection its folder.
--- Otherwise, without an argument: the buffer's connection, else the list of connections.
function M.open(arg, bang)
  arg = vim.trim(arg or "")
  local tree = require("dbear.tree")
  if not bang and tree.enabled() and not arg:find("/", 1, true) and not arg:match("^%a[%w+.-]*:") then
    return tree.open(arg)
  end
  if arg == "" then
    local t = dbear().target()
    if t then
      return open({ kind = "conn", conn = t.connection })
    end
    return open({ kind = "root" })
  end
  if arg:match("^%a[%w+.-]*:") and not vim.startswith(arg, url.SCHEME) then
    -- A connection URL (postgres://…, sqlite:/path…): its own component, encoded.
    return open({ kind = "conn", conn = arg })
  end
  local name = vim.startswith(arg, url.SCHEME) and arg or url.SCHEME .. arg
  if not url.parse(name) then
    return notify("not a dbear:// path: " .. arg, vim.log.levels.ERROR)
  end
  vim.cmd.edit(vim.fn.fnameescape(name))
end

--- The table buffer's state (or nil), for commands.
local function current_table()
  local st = grid.states[vim.api.nvim_get_current_buf()]
  if not st or not st.ready then
    notify("not a dbear table buffer", vim.log.levels.WARN)
  end
  return st and st.ready and st or nil
end

--- `:DbearSort [col [desc], …]`; no argument: natural order.
function M.sort(arg)
  local st = current_table()
  if not st then
    return
  end
  local sort = {}
  for part in (arg or ""):gmatch("[^,]+") do
    local col, dir = vim.trim(part):match("^(%S+)%s*(%a*)$")
    if not col then
      return notify("usage: :DbearSort column [asc|desc], …", vim.log.levels.ERROR)
    end
    if not st.index[col] then
      return notify("no column " .. col, vim.log.levels.ERROR)
    end
    sort[#sort + 1] = { column = col, descending = dir:lower() == "desc" }
  end
  grid.set_sort(st, sort)
end

--- `:DbearFilter [expr]`; no argument: none.
function M.filter(arg)
  local st = current_table()
  if st then
    grid.set_filter(st, vim.trim(arg or ""))
  end
end

--- Column names of the current table (for completion).
function M.columns()
  local st = grid.states[vim.api.nvim_get_current_buf()]
  return st and st.columns and vim.tbl_map(function(c) return c.name end, st.columns) or {}
end

return M
