--- Turns protocol results into lines for a buffer. Pure functions: they return lines and
--- highlights (`{ line (0-based), start_col, end_col (bytes), group }`), and draw nothing.

local M = {}

M.opts = { null = "NULL", max_width = 60 }

--- A cell's text and kind ("null", "number", "bool" or "text"). See PROTOCOL.md's Value table.
function M.value(v)
  if v == nil or v == vim.NIL then
    return M.opts.null, "null"
  end
  local t = type(v)
  if t == "boolean" then
    return tostring(v), "bool"
  elseif t == "number" then -- always an integer up to ±(2^53−1)
    return string.format("%d", v), "number"
  elseif t == "table" then
    if v.int then
      return v.int, "number"
    elseif v.decimal then
      return v.decimal, "number"
    elseif v.float ~= nil then
      local f = v.float
      if type(f) ~= "number" then
        return tostring(f), "number" -- "NaN", "Infinity", "-Infinity"
      end
      local s = string.format("%.15g", f)
      if tonumber(s) ~= f then
        s = string.format("%.17g", f)
      end
      return s, "number"
    end
    return vim.json.encode(v), "text"
  end
  return tostring(v), "text"
end

local escapes = { ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }

--- One line, control characters escaped, cut to `max` display cells with "…".
function M.clean(s, max)
  s = s:gsub("%c", function(c) return escapes[c] or string.format("\\x%02x", c:byte()) end)
  max = max or M.opts.max_width
  if not max or max <= 0 or vim.api.nvim_strwidth(s) <= max then
    return s
  end
  local out, width = {}, 0
  for _, ch in ipairs(vim.fn.split(s, "\\zs")) do
    local w = vim.api.nvim_strwidth(ch)
    if width + w > max - 1 then
      break
    end
    out[#out + 1], width = ch, width + w
  end
  return table.concat(out) .. "…"
end

local function pad(s, width, right)
  local gap = string.rep(" ", width - vim.api.nvim_strwidth(s))
  return right and gap .. s or s .. gap
end

--- An aligned grid. `rows` hold cells `{ text, kind }` (or plain strings). Numbers are right-aligned.
--- @return string[] lines, table[] highlights (line numbers relative to the first grid line)
function M.grid(headers, rows)
  local widths, cells, numeric = {}, {}, {}
  for c, h in ipairs(headers) do
    headers[c] = M.clean(h)
    widths[c] = vim.api.nvim_strwidth(headers[c])
  end
  for r, row in ipairs(rows) do
    cells[r] = {}
    for c = 1, #headers do
      local cell = row[c]
      if type(cell) ~= "table" then
        cell = { cell == nil and "" or tostring(cell), "text" }
      end
      local text = cell[2] == "null" and cell[1] or M.clean(cell[1])
      cells[r][c] = { text, cell[2] }
      widths[c] = math.max(widths[c], vim.api.nvim_strwidth(text))
      -- A column is numeric (right-aligned) when it holds numbers and NULLs only.
      if cell[2] == "number" and numeric[c] == nil then
        numeric[c] = true
      elseif cell[2] ~= "number" and cell[2] ~= "null" then
        numeric[c] = false
      end
    end
  end
  local lines, hls = {}, {}
  local head, rule = {}, {}
  for c, h in ipairs(headers) do
    head[c] = " " .. pad(h, widths[c], numeric[c]) .. " "
    rule[c] = string.rep("─", widths[c] + 2)
  end
  lines[1] = table.concat(head, "│")
  hls[1] = { 0, 0, -1, "DbearHeader" }
  lines[2] = table.concat(rule, "┼")
  for r, row in ipairs(cells) do
    local parts, col = {}, 0
    for c, cell in ipairs(row) do
      local part = " " .. pad(cell[1], widths[c], numeric[c]) .. " "
      if cell[2] == "null" then -- dimmed, so it never reads as the text "NULL"
        local start = numeric[c] and col + #part - #cell[1] - 1 or col + 1
        hls[#hls + 1] = { r + 1, start, start + #cell[1], "DbearNull" }
      end
      parts[c] = part
      col = col + #part + #"│"
    end
    lines[r + 2] = table.concat(parts, "│")
  end
  return lines, hls
end

local function plural(n, word)
  return string.format("%d %s%s", n, word, n == 1 and "" or "s")
end

--- Appends `more` lines (and their highlights, shifted) to `lines`/`hls`.
local function append(lines, hls, more, more_hls)
  local base = #lines
  for _, h in ipairs(more_hls or {}) do
    hls[#hls + 1] = { h[1] + base, h[2], h[3], h[4] }
  end
  vim.list_extend(lines, more)
end

--- A `query.run` result. The first line says rows, duration or the error.
function M.outcome(o)
  local ms = string.format("%d ms", o.duration_ms or 0)
  local res = o.result or {}
  local lines, hls = {}, {}
  if res.error then
    local e = res.error
    if e.kind == "cancelled" then
      return { "Cancelled · " .. ms }, { { 0, 0, -1, "DbearError" } }
    end
    local msg = vim.split(e.message ~= vim.NIL and e.message or e.kind, "\n", { plain = true })
    lines[1] = "Error: " .. msg[1] .. " · " .. ms
    hls[1] = { 0, 0, -1, "DbearError" }
    vim.list_extend(lines, msg, 2)
    return lines, hls
  end
  local r = res.ok
  if #r.columns == 0 then
    local n = r.rows_affected ~= vim.NIL and r.rows_affected or 0
    lines[1] = plural(n, "row") .. " affected · " .. ms
  else
    local summary = plural(#r.rows, "row")
    if r.truncated then
      local total = r.total_count ~= vim.NIL and r.total_count
      summary = "first " .. summary .. (total and " of " .. total or "") .. " (max rows)"
    end
    lines[1] = summary .. " · " .. ms
  end
  if o.schema_changed then
    lines[1] = lines[1] .. " · schema changed"
  end
  hls[1] = { 0, 0, -1, "DbearSummary" }
  if #r.columns > 0 then
    local headers, rows = {}, {}
    for i, c in ipairs(r.columns) do
      headers[i] = c.name
    end
    for i, row in ipairs(r.rows) do
      rows[i] = {}
      for c = 1, #headers do
        rows[i][c] = { M.value(row[c]) }
      end
    end
    local g, ghl = M.grid(headers, rows)
    append(lines, hls, g, ghl)
  end
  return lines, hls
end

--- A `schemas.list` result.
function M.tables(res)
  local rows = {}
  for _, schema in ipairs(res.schemas) do
    for _, t in ipairs(schema.tables) do
      local n = t.estimated_row_count
      rows[#rows + 1] = { t.schema, t.name, t.kind, n ~= vim.NIL and n ~= nil and { tostring(n), "number" } or "" }
    end
  end
  local lines, hls = { plural(#rows, "table") .. " · " .. res.target.database }, { { 0, 0, -1, "DbearSummary" } }
  local g, ghl = M.grid({ "schema", "name", "kind", "rows" }, rows)
  append(lines, hls, g, ghl)
  return lines, hls
end

local function list(names)
  return table.concat(names, ", ")
end

--- A `table.describe` result: columns, then indexes, foreign keys, referencing tables and the DDL.
function M.describe(res)
  local t, s = res.table, res.structure
  local lines, hls = { string.format("%s.%s (%s)", t.schema, t.name, t.kind) }, { { 0, 0, -1, "DbearSummary" } }
  local rows = {}
  for i, c in ipairs(s.columns) do
    local opt = function(v) return v ~= vim.NIL and v or "" end
    rows[i] = { c.name, c.type_name, c.is_nullable and "yes" or "no", opt(c.default_value), c.is_primary_key and "PK" or "", opt(c.comment) }
  end
  local g, ghl = M.grid({ "column", "type", "nullable", "default", "key", "comment" }, rows)
  append(lines, hls, g, ghl)
  local function section(title, items)
    if #items > 0 then
      lines[#lines + 1] = ""
      lines[#lines + 1] = title
      hls[#hls + 1] = { #lines - 1, 0, -1, "DbearHeader" }
      for _, item in ipairs(items) do
        lines[#lines + 1] = "  " .. item
      end
    end
  end
  local idx, fks, refs = {}, {}, {}
  for _, i in ipairs(s.indexes) do
    idx[#idx + 1] = i.definition ~= vim.NIL and i.definition
      or string.format("%s%s (%s)", i.name, i.is_primary and " PRIMARY KEY" or i.is_unique and " UNIQUE" or "", list(i.columns))
  end
  for _, f in ipairs(s.foreign_keys) do
    fks[#fks + 1] = string.format("%s (%s) → %s.%s (%s)", f.name, list(f.columns), f.referenced_schema, f.referenced_table, list(f.referenced_columns))
  end
  for _, r in ipairs(s.referenced_by) do
    refs[#refs + 1] = string.format("%s.%s %s (%s)", r.schema, r.table, r.name, list(r.columns))
  end
  section("Indexes:", idx)
  section("Foreign keys:", fks)
  section("Referenced by:", refs)
  if s.ddl ~= vim.NIL and s.ddl then
    section("DDL:", vim.split(s.ddl, "\n", { plain = true }))
  end
  return lines, hls
end

return M
