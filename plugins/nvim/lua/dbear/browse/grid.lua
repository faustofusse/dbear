--- A table's rows as an editable buffer (`dbear://<conn>/[<db>/]<schema>.<table>`, or a table file
--- of the mirror, see `dbear.tree`: then `spec.dir` is its folder, where `-` goes).
---
--- The buffer text is the source of truth: `diff.compute` compares it with the rows as loaded
--- (`st.rows`, by the concealed row id at the start of each line) whenever it changes, to keep
--- 'modified' honest, and on `:w`, whose changes go to the server for SQL (`table.changes.preview`),
--- a confirmation float, and `table.changes.apply` (one transaction). Lua never builds SQL.
---
--- Loading rows (first page, more pages, reloads) never leaves an undo step behind: undoing it
--- would make rows vanish, i.e. look deleted. Realigning after an edit joins the edit's undo step.

local cells = require("dbear.browse.cells")
local diff = require("dbear.browse.diff")
local ui = require("dbear.browse.ui")
local url = require("dbear.browse.url")
local render = require("dbear.render")

local M = {}

--- buffer → state
M.states = {}

-- Row ids are unique across buffers and reloads, so a line pasted from another table buffer (or
-- from before a reload) is never taken for a row of this one: it's a new row.
local next_id = 1

local ns_deco = vim.api.nvim_create_namespace("dbear.browse.cells")
local ns_status = vim.api.nvim_create_namespace("dbear.browse.status")
local ns_footer = vim.api.nvim_create_namespace("dbear.browse.footer")
local ns_track = vim.api.nvim_create_namespace("dbear.browse.track")
M.ns_diagnostics = vim.api.nvim_create_namespace("dbear.browse.diagnostics")

local function dbear()
  return require("dbear")
end

local function config()
  return dbear().config.browse
end

local function notify(msg, level)
  vim.notify("dbear: " .. msg, level or vim.log.levels.INFO)
end

local function nonnil(v)
  if v == vim.NIL then
    return nil
  end
  return v
end

local function lines_of(buf)
  return vim.api.nvim_buf_get_lines(buf, 0, -1, false)
end

--- Runs `fn` (buffer changes) without leaving an undo step: the history is cleared instead.
local function without_undo(buf, fn)
  local ul = vim.bo[buf].undolevels
  local modifiable = vim.bo[buf].modifiable
  vim.bo[buf].undolevels = -1
  vim.bo[buf].modifiable = true
  local ok, err = pcall(fn)
  vim.bo[buf].undolevels = ul
  vim.bo[buf].modifiable = modifiable
  if not ok then
    error(err, 0)
  end
end

local function params(st, extra)
  local p = { connection = st.spec.conn, database = st.spec.db, schema = st.schema, table = st.table }
  return vim.tbl_extend("force", p, extra or {})
end

local function has_pending(st)
  return st.diff ~= nil and (#st.diff.changes > 0 or #st.diff.errors > 0)
end
M.has_pending = has_pending

-- MARK: Rows and columns

local numeric_type = { "int", "real", "floa", "doub", "num", "dec", "money", "serial" }

local function is_numeric_type(t)
  t = (t or ""):lower()
  for _, n in ipairs(numeric_type) do
    if t:find(n, 1, true) and not t:find("interval", 1, true) and not t:find("point", 1, true) then
      return true
    end
  end
  return false
end

local function setup_columns(st, editable, result_columns)
  local by_name = {}
  for _, c in ipairs(editable.columns) do
    by_name[c.name] = c
  end
  if #result_columns == 0 then
    result_columns = editable.columns
  end
  st.columns, st.index = {}, {}
  for i, rc in ipairs(result_columns) do
    local e = by_name[rc.name] or { type_name = rc.type_name, read_only = "not a column of the table" }
    st.columns[i] = {
      name = rc.name,
      type = e.type_name,
      pk = e.is_primary_key or false,
      binary = e.binary or false,
      read_only = nonnil(e.read_only),
      default = nonnil(e.default_value),
    }
    st.index[rc.name] = i
  end
  st.read_only = nonnil(editable.read_only)
  st.key = editable.key or {}
  for _, k in ipairs(st.key) do
    if not st.index[k] then
      st.read_only = "the primary key isn't in the rows"
    end
  end
  st.foreign_keys, st.referenced_by = editable.foreign_keys or {}, editable.referenced_by or {}
end

local function make_row(st, id, values)
  local row = { id = id, values = values, cells = {}, cut = {}, texts = {} }
  local max = config().max_width
  for c, col in ipairs(st.columns) do
    local text, kind = render.value(values[c])
    if kind == "null" then
      row.cells[c], row.cut[c] = "\\N", false
    else
      row.texts[c] = text
      local shown, cut = cells.cut(cells.escape(text), max)
      row.cells[c], row.cut[c] = shown, cut and not col.binary
    end
  end
  return row
end

local function add_rows(st, rows)
  local lines = {}
  for _, values in ipairs(rows) do
    local id = next_id
    next_id = id + 1
    local row = make_row(st, id, values)
    st.rows[id] = row
    st.order[#st.order + 1] = id
    lines[#lines + 1] = cells.format(id, row.cells, st.widths or {}, st.numeric, st.idw or 3)
  end
  return lines
end

-- MARK: Layout

--- Re-pads every readable line to the columns' widths (lines that can't be read stay as they
--- are), keeping the cursor in its cell. Joins the last undo step unless `no_undo`.
local function realign(st, no_undo)
  local buf = st.buf
  local lines = lines_of(buf)
  local ncols = #st.columns
  local max = config().max_width
  local widths, parsed = {}, {}
  for c, col in ipairs(st.columns) do
    widths[c] = cells.width(col.name)
  end
  for i, line in ipairs(lines) do
    local p = cells.parse(line, ncols)
    if p then
      parsed[i] = p
      for c, t in ipairs(p.cells) do
        widths[c] = math.max(widths[c], math.min(cells.width(t), max))
      end
    end
  end
  st.widths = widths
  st.idw = math.max(3, #tostring(next_id - 1))
  local names = {}
  for c, col in ipairs(st.columns) do
    names[c] = col.name
  end
  st.header = cells.header(names, widths, st.numeric)
  local new = {}
  for i, p in pairs(parsed) do
    local l = cells.format(p.id, p.cells, widths, st.numeric, st.idw)
    if l ~= lines[i] then
      new[i] = l
    end
  end
  if next(new) == nil then
    return
  end
  local cursors = {}
  for _, win in ipairs(vim.fn.win_findbuf(buf)) do
    local r, col = unpack(vim.api.nvim_win_get_cursor(win))
    if new[r] and parsed[r] then
      local k, off = cells.locate(lines[r], parsed[r], col)
      cursors[win] = { r, k, off }
    end
  end
  local function apply()
    for i, l in pairs(new) do
      if not no_undo then
        vim.cmd("silent! undojoin")
      end
      vim.api.nvim_buf_set_lines(buf, i - 1, i, false, { l })
    end
  end
  if no_undo then
    without_undo(buf, apply)
  else
    apply()
  end
  for win, c in pairs(cursors) do
    local line = new[c[1]]
    local p = cells.parse(line, ncols)
    if p then
      pcall(vim.api.nvim_win_set_cursor, win, { c[1], cells.position(line, p, c[2], c[3]) })
    end
  end
end

local function sort_text(st)
  local parts = {}
  for _, k in ipairs(st.query.sort or {}) do
    parts[#parts + 1] = k.column .. (k.descending and " ↓" or " ↑")
  end
  return table.concat(parts, ", ")
end

local function footer(st)
  local buf = st.buf
  vim.api.nvim_buf_clear_namespace(buf, ns_footer, 0, -1)
  local parts = { string.format("%d row%s", #st.order, #st.order == 1 and "" or "s") }
  if st.loading then
    parts[#parts + 1] = "loading…"
  elseif st.next then
    parts[#parts + 1] = "more below (]])"
  end
  if #(st.query.sort or {}) > 0 then
    parts[#parts + 1] = "sort " .. sort_text(st)
  end
  if st.query.filter then
    parts[#parts + 1] = "where " .. st.query.filter
  end
  if st.diff and #st.diff.changes > 0 then
    parts[#parts + 1] = "pending: " .. diff.summary(st.diff.counts)
  end
  if st.diff and #st.diff.errors > 0 then
    parts[#parts + 1] = string.format("%d line%s can't be read", #st.diff.errors, #st.diff.errors == 1 and "" or "s")
  end
  local virt = { { { "── " .. table.concat(parts, " · "), "DbearSummary" } } }
  if st.read_only then
    virt[#virt + 1] = { { "read-only: " .. st.read_only, "DbearReadOnly" } }
  end
  if st.error then
    for i, l in ipairs(vim.split(st.error, "\n", { plain = true })) do
      virt[#virt + 1] = { { (i == 1 and "save failed: " or "  ") .. l, "DbearError" } }
    end
  end
  local last = vim.api.nvim_buf_line_count(buf) - 1
  vim.api.nvim_buf_set_extmark(buf, ns_footer, last, 0, { virt_lines = virt })
end

--- Recomputes the pending changes: 'modified', the signs, the diagnostics and the footer.
local function refresh(st)
  local buf = st.buf
  if not st.ready or not vim.api.nvim_buf_is_valid(buf) then
    return
  end
  local d = diff.compute(st, lines_of(buf))
  st.diff = d
  vim.bo[buf].modified = has_pending(st)
  vim.api.nvim_buf_clear_namespace(buf, ns_status, 0, -1)
  for lnum, s in pairs(d.status) do
    local sign = s == "update" and "~" or "+"
    local hl = s == "update" and "DbearUpdate" or "DbearInsert"
    pcall(vim.api.nvim_buf_set_extmark, buf, ns_status, lnum - 1, 0, { sign_text = sign, sign_hl_group = hl })
  end
  local items = {}
  for _, e in ipairs(d.errors) do
    items[#items + 1] = { lnum = e.lnum - 1, col = 0, message = e.msg, severity = vim.diagnostic.severity.ERROR, source = "dbear" }
  end
  vim.diagnostic.set(M.ns_diagnostics, buf, items)
  footer(st)
end
M.refresh = refresh

--- After an edit in Normal mode (or leaving Insert mode).
local function on_change(st)
  if not st.ready or st.busy then
    return
  end
  local tick = vim.b[st.buf].changedtick
  if st.tick == tick then
    return
  end
  local ut = vim.fn.undotree()
  -- Not on an undo/redo: realigning would start a new branch and lose the redo.
  if ut.seq_cur == ut.seq_last and vim.bo[st.buf].modifiable then
    realign(st)
  end
  st.tick = vim.b[st.buf].changedtick
  refresh(st)
end

-- MARK: Decorations (drawn per visible line, so they follow yanks, pastes and undo)

local function decorate(st, buf, row)
  local line = vim.api.nvim_buf_get_lines(buf, row, row + 1, false)[1]
  if not line then
    return
  end
  local id, plen = cells.prefix(line)
  if id == false then
    return
  end
  local mark = function(col, opts)
    opts.ephemeral = true
    pcall(vim.api.nvim_buf_set_extmark, buf, ns_deco, row, col, opts)
  end
  if plen > 0 then
    mark(0, { end_col = plen, conceal = "" })
  end
  local data = id and st.rows[id]
  for i, r in ipairs(cells.ranges(line, plen)) do
    if i > 1 then
      mark(r[1] - 1, { end_col = r[1], conceal = "│", hl_group = "DbearSeparator" })
    end
    local text = line:sub(r[1] + 1, r[2])
    local lead = #text:match("^ *")
    local t = text:gsub("^ +", ""):gsub(" +$", "")
    if #t > 0 then
      local s = r[1] + lead
      local group
      if data and st.columns[i] and t ~= data.cells[i] then
        group = "DbearChanged"
      elseif t == "\\N" or t == "\\D" then
        group = "DbearNull"
      elseif data and data.cut[i] then
        mark(s + #t - #"…", { end_col = s + #t, hl_group = "DbearCut" })
      end
      if group then
        mark(s, { end_col = s + #t, hl_group = group })
      end
    end
  end
end

vim.api.nvim_set_decoration_provider(ns_deco, {
  on_win = function(_, _, buf)
    local st = M.states[buf]
    return st ~= nil and st.ready == true
  end,
  on_line = function(_, _, buf, row)
    local st = M.states[buf]
    if st then
      decorate(st, buf, row)
    end
  end,
})

--- The header shown in the window bar, scrolled with the text.
function M.winbar()
  local win = vim.g.statusline_winid
  local st = M.states[vim.api.nvim_win_get_buf(win)]
  if not st or not st.header then
    return ""
  end
  local info = vim.fn.getwininfo(win)[1]
  local leftcol = vim.api.nvim_win_call(win, function() return vim.fn.winsaveview().leftcol end)
  local shift = math.max(0, leftcol - ((st.idw or 3) + 2))
  -- Cut to the window ourselves: a statusline too long loses its start, not its end.
  local room = math.max(0, info.width - info.textoff)
  local text = vim.fn.strcharpart(st.header, shift, room)
  while vim.api.nvim_strwidth(text) > room do
    text = vim.fn.strcharpart(text, 0, vim.fn.strchars(text) - 1)
  end
  return string.rep(" ", info.textoff) .. "%#DbearHeader#" .. text:gsub("%%", "%%%%")
end

-- MARK: Loading

--- Shows the first page of `res` (a `table.rows` result) and `editable`, replacing everything.
local function show_first(st, editable, res)
  st.schema, st.table = res.table.schema, res.table.name
  setup_columns(st, editable, res.result.columns)
  st.rows, st.order, st.allowed = {}, {}, {}
  st.numeric = {}
  for c, col in ipairs(st.columns) do
    local numeric = is_numeric_type(col.type)
    if not numeric then -- typeless (SQLite): numbers and NULLs only, at least one number
      local numbers = 0
      for _, row in ipairs(res.result.rows) do
        local _, kind = render.value(row[c])
        if kind == "number" then
          numbers = numbers + 1
        elseif kind ~= "null" then
          numbers = -math.huge
        end
      end
      numeric = numbers > 0
    end
    st.numeric[c] = numeric
  end
  st.next = nonnil(res.next)
  st.widths, st.idw = nil, 3
  local lines = add_rows(st, res.result.rows)
  local buf = st.buf
  local cursor = vim.fn.bufwinid(buf) ~= -1 and vim.api.nvim_win_get_cursor(vim.fn.bufwinid(buf)) or nil
  st.busy = true
  without_undo(buf, function()
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, #lines > 0 and lines or { "" })
  end)
  st.ready = true
  realign(st, true)
  st.busy = false
  st.error = nil
  vim.bo[buf].modifiable = st.read_only == nil
  st.tick = vim.b[buf].changedtick
  vim.diagnostic.reset(M.ns_diagnostics, buf)
  refresh(st)
  if cursor then
    local win = vim.fn.bufwinid(buf)
    local row = math.min(cursor[1], vim.api.nvim_buf_line_count(buf))
    pcall(vim.api.nvim_win_set_cursor, win, { row, cursor[2] })
  end
end

--- Loads the first page (again), with `query` (default: the current one). Asks before
--- discarding pending changes unless `force`.
function M.reload(st, query, force)
  if st.loading then
    return notify("still loading", vim.log.levels.WARN)
  end
  if not force and has_pending(st) and not ui.ask("Discard the pending changes to " .. st.table .. "?") then
    return
  end
  query = query or st.query
  st.loading = true
  if st.ready then
    footer(st)
  end
  local rpc = dbear().rpc()
  local editable, page, failed
  local function done()
    if not (editable and page) and not failed then
      return
    end
    st.loading = false
    if failed then
      if st.ready then
        footer(st)
      else
        without_undo(st.buf, function() vim.api.nvim_buf_set_lines(st.buf, 0, -1, false, { "Error: " .. (failed.message or "?") }) end)
        vim.bo[st.buf].modified = false
      end
      return notify(failed.message or vim.inspect(failed), vim.log.levels.ERROR)
    end
    if not vim.api.nvim_buf_is_valid(st.buf) then
      return
    end
    st.query = query
    show_first(st, editable, page)
    if st.on_load then
      st.on_load(st)
    end
  end
  local p = params(st, { schema = st.schema or st.spec.schema, table = st.table or st.spec.table })
  rpc:request("table.editable", p, function(err, res)
    failed = failed or err
    editable = res
    done()
  end)
  rpc:request("table.rows", vim.tbl_extend("force", p, { query = query, limit = config().page_size }), function(err, res)
    failed = failed or err
    page = res
    done()
  end)
end

--- Appends the next page. Rows loaded while changes are pending keep them (the undo history
--- starts again, since undoing a load would delete rows).
function M.load_more(st)
  if not st.next then
    return notify("all rows are loaded")
  end
  if st.loading then
    return
  end
  st.loading = true
  footer(st)
  local pending = has_pending(st)
  dbear().rpc():request("table.rows", params(st, { query = st.query, limit = config().page_size, cursor = st.next }), function(err, res)
    st.loading = false
    if not vim.api.nvim_buf_is_valid(st.buf) then
      return
    end
    if err then
      footer(st)
      return notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    end
    st.next = nonnil(res.next)
    local lines = add_rows(st, res.result.rows)
    st.busy = true
    without_undo(st.buf, function()
      local last = vim.api.nvim_buf_line_count(st.buf)
      local only = last == 1 and vim.api.nvim_buf_get_lines(st.buf, 0, 1, false)[1] == ""
      vim.api.nvim_buf_set_lines(st.buf, only and 0 or last, -1, false, lines)
    end)
    realign(st, true)
    st.busy = false
    st.tick = vim.b[st.buf].changedtick
    refresh(st)
    if pending then
      notify(string.format("loaded %d more rows; the undo history starts here", #lines))
    end
  end)
end

-- MARK: Saving

local function error_text(errors)
  local out = {}
  for _, e in ipairs(errors) do
    out[#out + 1] = string.format("line %d: %s", e.lnum, e.msg)
  end
  return table.concat(out, "\n")
end

--- `:w`: the changes → the server's SQL → confirmation → one transaction → reload.
function M.save(st)
  if st.read_only then
    return notify("read-only: " .. st.read_only, vim.log.levels.WARN)
  end
  if st.saving or st.loading then
    return notify("busy, try again", vim.log.levels.WARN)
  end
  refresh(st)
  local d = st.diff
  if #d.errors > 0 then
    -- Scheduled: an error inside BufWriteCmd would also be reported as the autocommand's.
    local text = error_text(d.errors)
    return vim.schedule(function() notify("nothing saved, fix these lines first:\n" .. text, vim.log.levels.ERROR) end)
  end
  if #d.changes == 0 then
    vim.bo[st.buf].modified = false
    return notify("no changes")
  end
  st.saving = true
  local rpc = dbear().rpc()
  local p = params(st, { changes = d.changes })
  rpc:request("table.changes.preview", p, function(err, res)
    if err then
      st.saving = false
      return notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    end
    local summary = diff.summary(d.counts) .. " in " .. st.schema .. "." .. st.table
    local rekeyed = 0
    for _, c in ipairs(d.changes) do
      for _, e in ipairs(c.set or {}) do
        if vim.tbl_contains(st.key, e.column) then
          rekeyed = rekeyed + 1
          break
        end
      end
    end
    if rekeyed > 0 then
      summary = summary .. string.format(" · changes the primary key of %d row%s", rekeyed, rekeyed == 1 and "" or "s")
    end
    st.confirm_win = ui.confirm(summary, res.statements, function(yes)
      st.confirm_win = nil
      if not yes then
        st.saving = false
        return notify("not saved")
      end
      local reviewed = vim.tbl_map(function(s) return s.sql end, res.statements)
      rpc:request("table.changes.apply", vim.tbl_extend("force", p, { reviewed = reviewed }), function(aerr, ares)
        st.saving = false
        if aerr then
          st.error = aerr.message or vim.inspect(aerr)
          footer(st)
          return notify(st.error, vim.log.levels.ERROR)
        end
        notify(string.format("saved (%d row%s)", ares.rows_affected, ares.rows_affected == 1 and "" or "s"))
        M.reload(st, nil, true)
      end)
    end)
  end)
end

-- MARK: Cells, keys and links

--- The line under the cursor, parsed, and the cell the cursor is in.
local function cursor_cell(st)
  local r, col = unpack(vim.api.nvim_win_get_cursor(0))
  local line = vim.api.nvim_buf_get_lines(st.buf, r - 1, r, false)[1]
  local p, msg = cells.parse(line, #st.columns)
  if not p then
    return nil, msg or "not a row"
  end
  local k = cells.locate(line, p, col)
  return p, k, r
end

--- `<CR>`: the cell's whole value in a buffer of its own; `:w` there puts it back in the row.
function M.edit_cell(st)
  local p, k, r = cursor_cell(st)
  if not p then
    return notify("can't read this line: " .. k, vim.log.levels.WARN)
  end
  local column = st.columns[k]
  local row = p.id and st.rows[p.id]
  local raw = p.cells[k]
  local text, null
  if row and raw == row.cells[k] then
    text, null = row.texts[k] or "", row.texts[k] == nil
  else
    local d, msg = cells.decode(raw)
    if not d then
      return notify(column.name .. ": " .. msg, vim.log.levels.WARN)
    end
    text, null = d.text or "", d.kind == "null"
  end
  local editable = not st.read_only and not column.binary and not column.read_only
  local mark = vim.api.nvim_buf_set_extmark(st.buf, ns_track, r - 1, 0, { invalidate = true })
  st.cell_serial = (st.cell_serial or 0) + 1
  local ebuf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(ebuf, string.format("dbear-cell://%s.%s/%s#%d", st.schema, st.table, column.name, st.cell_serial))
  vim.api.nvim_buf_set_lines(ebuf, 0, -1, false, null and {} or vim.split(text, "\n", { plain = true }))
  vim.bo[ebuf].buftype = editable and "acwrite" or "nofile"
  vim.bo[ebuf].bufhidden = "wipe"
  vim.bo[ebuf].modified = false
  vim.bo[ebuf].modifiable = editable
  vim.b[ebuf].dbear_cell = { column = column.name, null = null }
  vim.cmd("belowright " .. math.min(12, math.max(3, #vim.api.nvim_buf_get_lines(ebuf, 0, -1, false) + 1)) .. "split")
  vim.api.nvim_win_set_buf(0, ebuf)
  vim.wo[0][0].winbar = string.format("%%#DbearHeader#%s%s", column.name:gsub("%%", "%%%%"),
    editable and " · :w puts it in the row (then :w the table to save)" .. (null and " · was NULL" or "")
      or " · read-only" .. (column.binary and " (binary)" or ""))
  if not editable then
    return
  end
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    buffer = ebuf,
    callback = function()
      local pos = vim.api.nvim_buf_get_extmark_by_id(st.buf, ns_track, mark, { details = true })
      if #pos == 0 or (pos[3] and pos[3].invalid) then
        return notify("the row is gone from the table buffer", vim.log.levels.ERROR)
      end
      local elines = vim.api.nvim_buf_get_lines(ebuf, 0, -1, false)
      local value = table.concat(elines, "\n")
      local esc
      if null and value == "" then
        esc = "\\N" -- left empty: still NULL
      else
        esc = value == "" and "\\e" or cells.escape(value)
      end
      local lnum = pos[1]
      local line = vim.api.nvim_buf_get_lines(st.buf, lnum, lnum + 1, false)[1]
      local lp = cells.parse(line, #st.columns)
      if not lp then
        return notify("the row's line can't be read any more", vim.log.levels.ERROR)
      end
      local range = lp.ranges[k]
      local updated = line:sub(1, range[1]) .. " " .. esc .. " " .. line:sub(range[2] + 1)
      local lrow = lp.id and st.rows[lp.id]
      if lrow and lrow.cut[k] and esc ~= lrow.cells[k] then
        st.allowed[lp.id] = st.allowed[lp.id] or {}
        st.allowed[lp.id][k] = esc
      end
      vim.api.nvim_buf_call(st.buf, function()
        vim.api.nvim_buf_set_lines(st.buf, lnum, lnum + 1, false, { updated })
        realign(st)
        st.tick = vim.b[st.buf].changedtick
        refresh(st)
      end)
      vim.bo[ebuf].modified = false
      notify("put in the row; :w the table to save")
    end,
  })
end

local function open(spec)
  vim.cmd.edit(vim.fn.fnameescape(url.format(spec)))
end

local function follow(st, schema, table, columns, values)
  for _, v in ipairs(values) do
    if v == vim.NIL then
      return notify("the key is NULL: nothing to follow", vim.log.levels.WARN)
    end
  end
  local p = { connection = st.spec.conn, database = st.spec.db, schema = schema, table = table, columns = columns, values = values }
  dbear().rpc():request("table.related", p, function(err, res)
    if err then
      return notify(err.message or vim.inspect(err), vim.log.levels.ERROR)
    end
    open({ kind = "table", conn = st.spec.conn, db = st.spec.db, schema = res.table.schema, table = res.table.name, filter = res.filter })
    -- Reached from the mirror: `-` goes back to the mirror's folders.
    local opened = M.states[vim.api.nvim_get_current_buf()]
    if opened and st.spec.mirror then
      opened.spec.mirror = true
    end
  end)
end

local function loaded_row(st)
  local p, k = cursor_cell(st)
  if not p then
    notify("can't read this line: " .. k, vim.log.levels.WARN)
    return nil
  end
  local row = p.id and st.rows[p.id]
  if not row then
    notify("a new row: save it first", vim.log.levels.WARN)
    return nil
  end
  return row, k
end

local function pick(items, prompt, fn)
  if #items == 1 then
    return fn(items[1])
  end
  vim.ui.select(items, { prompt = prompt, format_item = function(i) return i.label end }, function(i)
    if i then
      fn(i)
    end
  end)
end

--- `gf`: the row a foreign key of the cell under the cursor points at.
function M.follow_key(st)
  local row, k = loaded_row(st)
  if not row then
    return
  end
  local name = st.columns[k].name
  local here = vim.tbl_filter(function(fk) return vim.tbl_contains(fk.columns, name) end, st.foreign_keys)
  if #here == 0 then
    here = st.foreign_keys
  end
  if #here == 0 then
    return notify("no foreign key here", vim.log.levels.WARN)
  end
  pick(here, "Follow", function(fk)
    local values = vim.tbl_map(function(c) return row.values[st.index[c]] end, fk.columns)
    follow(st, fk.schema, fk.table, fk.target_columns, values)
  end)
end

--- `gR`: the rows of other tables that reference this row.
function M.referenced_by(st)
  local row = loaded_row(st)
  if not row then
    return
  end
  if #st.referenced_by == 0 then
    return notify("no table references " .. st.table, vim.log.levels.WARN)
  end
  pick(st.referenced_by, "Referenced by", function(ref)
    local values = vim.tbl_map(function(c) return row.values[st.index[c]] end, ref.values)
    follow(st, ref.schema, ref.table, ref.columns, values)
  end)
end

--- Sorts by `sort` (`{ { column, descending } }`, empty: natural order), reloading.
function M.set_sort(st, sort)
  M.reload(st, { sort = sort, filter = st.query.filter })
end

--- Filters with a raw SQL `WHERE` expression (nil or "": none), reloading.
function M.set_filter(st, filter)
  M.reload(st, { sort = st.query.sort, filter = filter ~= "" and filter or nil })
end

--- `gs`: sort by the column under the cursor: ascending, descending, then not.
function M.cycle_sort(st)
  local _, k = cursor_cell(st)
  if type(k) ~= "number" then
    return notify("put the cursor on a cell", vim.log.levels.WARN)
  end
  local name = st.columns[k].name
  local first = (st.query.sort or {})[1]
  local sort
  if first and first.column == name and not first.descending then
    sort = { { column = name, descending = true } }
  elseif first and first.column == name then
    sort = {}
  else
    sort = { { column = name, descending = false } }
  end
  M.set_sort(st, sort)
end

--- `o`/`O`: a new row (every cell empty: DEFAULT), in Insert mode in its first non-key cell.
function M.new_row(st, above)
  if st.read_only then
    return notify("read-only: " .. st.read_only, vim.log.levels.WARN)
  end
  local r = vim.api.nvim_win_get_cursor(0)[1]
  local empty = {}
  for c = 1, #st.columns do
    empty[c] = ""
  end
  local line = cells.format(0, empty, st.widths, st.numeric, st.idw)
  local at = above and r - 1 or r
  vim.api.nvim_buf_set_lines(st.buf, at, at, false, { line })
  local p = cells.parse(line, #st.columns)
  local k = 1
  for c, col in ipairs(st.columns) do
    if not col.pk then
      k = c
      break
    end
  end
  local range = p.ranges[k]
  vim.api.nvim_win_set_cursor(0, { at + 1, range[1] + (k > 1 and 1 or 0) })
  vim.cmd("startinsert")
end

--- Keeps the cursor off the concealed row id: on it, the cursor goes to the first cell's text.
local function clamp(st)
  local r, col = unpack(vim.api.nvim_win_get_cursor(0))
  local line = vim.api.nvim_buf_get_lines(st.buf, r - 1, r, false)[1] or ""
  local id, plen = cells.prefix(line)
  if id ~= false and plen > 0 and col < plen and #line > plen then
    local lead = #line:sub(plen + 1):match("^ *")
    local first = line:sub(plen + lead + 1, plen + lead + 1)
    vim.api.nvim_win_set_cursor(0, { r, (first == "" or first == "|") and plen or plen + lead })
  end
end

M.keys = {
  { "<CR>", "edit_cell", "Edit the cell's whole value in a buffer of its own (:w puts it back)" },
  { "-", "parent", "Go up to the table list" },
  { "gf", "follow_key", "Open the row the foreign key under the cursor points at" },
  { "gR", "referenced_by", "Open the rows of other tables that reference this row" },
  { "gs", "cycle_sort", "Sort by the column under the cursor (ascending, descending, off)" },
  { "g/", "filter", "Filter with a SQL WHERE expression (empty: none)" },
  { "]]", "load_more", "Load more rows (also automatic near the end when nothing is pending)" },
  { "<C-l>", "refresh", "Reload (asks before discarding pending changes)" },
  { "gd", "describe", "Show the table's structure" },
  { "o / O", "new_row", "New row below/above (empty cells are DEFAULT)" },
  { "g?", "help", "This help" },
}

local function help(st)
  local lines = { "Keys" }
  for _, k in ipairs(M.keys) do
    lines[#lines + 1] = string.format("  %-6s %s", k[1], k[3])
  end
  vim.list_extend(lines, {
    "",
    "Editing (then :w shows the SQL to confirm)",
    "  change a cell = UPDATE · delete a line = DELETE",
    "  new line or a copy of a line (yyp) = INSERT",
    "  \\N NULL · \\D DEFAULT · \\e empty string · empty cell: '' (DEFAULT in new rows)",
    "  escapes: \\\\ \\| \\n \\r \\t \\s (space) \\xHH",
    "  cut cells (…) and binary values can't be edited in the line: use <CR>",
    "",
    ":DbearSort [col [desc], …]  :DbearFilter [expr]  :e! discards changes",
  })
  if st.read_only then
    table.insert(lines, 1, "Read-only: " .. st.read_only)
  end
  ui.info("dbear table", lines)
end

local actions = {
  edit_cell = M.edit_cell,
  parent = function(st)
    -- A table file of the mirror (or a filtered view reached from one): its folder.
    local dir = st.spec.dir or (st.spec.mirror and require("dbear.tree").dir_of(st.spec))
    if dir then
      return vim.cmd.edit(vim.fn.fnameescape(dir))
    end
    open(url.parent(st.spec))
  end,
  follow_key = M.follow_key,
  referenced_by = M.referenced_by,
  cycle_sort = M.cycle_sort,
  filter = function(st)
    vim.ui.input({ prompt = "where: ", default = st.query.filter or "" }, function(input)
      if input then
        M.set_filter(st, input)
      end
    end)
  end,
  load_more = M.load_more,
  refresh = function(st) M.reload(st) end,
  describe = function(st)
    dbear().rpc():request("table.describe", params(st), function(err, res)
      if err then
        return notify(err.message, vim.log.levels.ERROR)
      end
      dbear().show(render.describe(res))
    end)
  end,
  help = help,
}

--- Window options for a table buffer (set when it's shown).
local function window(win)
  vim.wo[win][0].conceallevel = 2
  vim.wo[win][0].concealcursor = "nvic"
  vim.wo[win][0].wrap = false
  vim.wo[win][0].list = false
  vim.wo[win][0].signcolumn = "auto"
  vim.wo[win][0].winbar = "%!v:lua.require'dbear.browse.grid'.winbar()"
end

--- Sets up `buf` as the table of `spec` and loads it.
function M.open(buf, spec)
  local st = M.states[buf]
  if st then -- :e! — discard and reload
    return M.reload(st, nil, true)
  end
  st = { buf = buf, spec = spec, query = { sort = spec.sort or {}, filter = spec.filter }, rows = {}, order = {}, allowed = {} }
  M.states[buf] = st
  vim.bo[buf].buftype = "acwrite"
  vim.bo[buf].swapfile = false
  -- Rows aren't a file's text: no modelines from them, and no .editorconfig (it runs again when
  -- the buffer is renamed, e.g. a mirror folder that moved, by the name: `Makefile`, `x.py`…).
  vim.bo[buf].modeline = false
  vim.b[buf].editorconfig = false
  vim.bo[buf].filetype = "dbear_table"
  vim.bo[buf].expandtab = true
  without_undo(buf, function() vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "Loading…" }) end)
  vim.bo[buf].modifiable = false
  vim.bo[buf].modified = false
  for _, k in ipairs(M.keys) do
    if actions[k[2]] then
      vim.keymap.set("n", k[1], function() actions[k[2]](st) end, { buffer = buf, nowait = true, desc = "dbear: " .. k[3] })
    end
  end
  -- `I` and `gI` insert before the first cell's text, never before the concealed row id.
  for _, lhs in ipairs({ "I", "gI" }) do
    vim.keymap.set("n", lhs, function()
      vim.api.nvim_win_set_cursor(0, { vim.api.nvim_win_get_cursor(0)[1], 0 })
      clamp(st)
      vim.cmd("startinsert")
    end, { buffer = buf, desc = "dbear: insert at the start of the row" })
  end
  vim.keymap.set("n", "o", function() M.new_row(st, false) end, { buffer = buf, desc = "dbear: new row below" })
  vim.keymap.set("n", "O", function() M.new_row(st, true) end, { buffer = buf, desc = "dbear: new row above" })
  local group = vim.api.nvim_create_augroup("dbear.browse.grid." .. buf, { clear = true })
  local au = function(event, fn) vim.api.nvim_create_autocmd(event, { group = group, buffer = buf, callback = fn }) end
  au("BufWriteCmd", function() M.save(st) end)
  au({ "TextChanged", "InsertLeave" }, function() on_change(st) end)
  au("InsertEnter", function()
    -- `I`, `0i`…: the cursor is placed after InsertEnter, so move it once it is.
    vim.schedule(function()
      if st.ready and vim.api.nvim_get_current_buf() == buf then
        clamp(st)
      end
    end)
  end)
  au({ "CursorMoved", "CursorMovedI" }, function()
    if not st.ready then
      return
    end
    clamp(st)
    local r = vim.api.nvim_win_get_cursor(0)[1]
    if st.next and not st.loading and not has_pending(st) and r >= vim.api.nvim_buf_line_count(buf) - 3 then
      M.load_more(st)
    end
  end)
  au("BufWinEnter", function() window(vim.api.nvim_get_current_win()) end)
  au("WinScrolled", function() vim.cmd("redrawstatus!") end)
  au("BufWipeout", function() M.states[buf] = nil end)
  for _, win in ipairs(vim.fn.win_findbuf(buf)) do
    window(win)
  end
  M.reload(st, nil, true)
end

return M
