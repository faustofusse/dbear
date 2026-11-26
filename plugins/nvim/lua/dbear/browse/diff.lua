--- A table buffer's lines against the rows as loaded → the row changes to send (`RowChange`s of
--- PROTOCOL.md) and the lines that can't be read. Pure: no buffers, no SQL.
---
--- Identity comes from each line's row id: a loaded id missing from the buffer is a DELETE; the
--- line that keeps an id is compared cell by cell (only cells whose text changed are written, as
--- an UPDATE); more lines with the same id are copies (INSERTs); lines without an id are new rows
--- (INSERTs), as are lines with another buffer's ids (pasted). Among lines sharing an id, the original is the first one still equal to the loaded
--- row, else the first one.

local cells = require("dbear.browse.cells")

local M = {}

--- The edit value (`EditValue` serde) of a decoded cell; `blank` is what an empty cell means.
local function edit_value(d, blank)
  if d.kind == "null" then
    return "null"
  elseif d.kind == "default" then
    return "default"
  elseif d.kind == "empty" then
    return blank
  end
  return { text = d.text }
end

local function same_value(v, text)
  -- v: an edit value; text: the loaded value's full text (nil for NULL)
  if v == "null" then
    return text == nil
  end
  return type(v) == "table" and text ~= nil and v.text == text
end

--- @param st table the buffer's state: `columns`, `rows`, `order`, `key`, `allowed`
--- @param lines string[]
function M.compute(st, lines)
  local ncols = #st.columns
  local errors, status = {}, {}
  local by_id, new, unreadable = {}, {}, {}
  local function fail(lnum, msg)
    errors[#errors + 1] = { lnum = lnum, msg = msg }
  end
  for lnum, line in ipairs(lines) do
    local p, msg = cells.parse(line, ncols)
    if p == false then
      fail(lnum, msg)
      -- Its row isn't deleted (nothing is saved while lines can't be read, but the count shows).
      local id = cells.prefix(line)
      if id then
        unreadable[id] = true
      end
    elseif p then
      p.lnum = lnum
      if p.id and st.rows[p.id] then
        by_id[p.id] = by_id[p.id] or {}
        table.insert(by_id[p.id], p)
      else -- no id, or another buffer's (pasted from there): a new row, as its text says
        new[#new + 1] = p
      end
    end
  end

  local changes, counts = {}, { update = 0, delete = 0, insert = 0 }
  local function key_of(row)
    local key = {}
    for _, name in ipairs(st.key) do
      key[#key + 1] = { column = name, value = row.values[st.index[name]] }
    end
    return key
  end
  local function column_error(lnum, col, msg)
    fail(lnum, string.format("%s: %s", st.columns[col].name, msg))
  end

  --- The value line `p` gives column `c`, or nil when the cell is unchanged (`source` given) or wrong.
  --- `blank`: what an empty cell means. `copy`: an untouched cell takes the source's value.
  local function cell(p, c, source, blank, copy)
    local raw, column = p.cells[c], st.columns[c]
    local allowed = source and st.allowed[source.id] and st.allowed[source.id][c]
    if source and raw == source.cells[c] and allowed ~= raw then
      if not copy then
        return nil, true
      end
      if source.texts[c] == nil then
        return "null", true
      end
      if column.binary then
        return column_error(p.lnum, c, "a binary value can't be copied (make the cell \\N or \\D)")
      end
      return { text = source.texts[c] }, true
    end
    if source and source.cut[c] and allowed ~= raw then
      return column_error(p.lnum, c, "the value is cut for display (…); edit it with <CR>, or undo the change")
    end
    local d, msg = cells.decode(raw)
    if not d then
      return column_error(p.lnum, c, msg)
    end
    local v = edit_value(d, blank)
    if column.binary and v ~= "null" and v ~= "default" then
      return column_error(p.lnum, c, "binary values can't be edited as text (only \\N or \\D)")
    end
    if column.read_only and not column.binary then
      return column_error(p.lnum, c, column.read_only)
    end
    return v, true
  end

  local function insert(p, source)
    local values, ok = {}, true
    for c = 1, ncols do
      local v, good = cell(p, c, source, "default", true)
      ok = ok and good
      values[c] = { column = st.columns[c].name, value = v }
    end
    if ok then
      changes[#changes + 1] = { kind = "insert", values = values }
      counts.insert = counts.insert + 1
      status[p.lnum] = source and "copy" or "insert"
    end
  end

  for _, id in ipairs(st.order) do
    local row, found = st.rows[id], by_id[id]
    if not found and not unreadable[id] then
      changes[#changes + 1] = { kind = "delete", key = key_of(row) }
      counts.delete = counts.delete + 1
    elseif found then
      local original = found[1]
      for _, p in ipairs(found) do
        if vim.deep_equal(p.cells, row.cells) then
          original = p
          break
        end
      end
      local set = {}
      for c = 1, ncols do
        local v, good = cell(original, c, row, { text = "" }, false)
        if good and v ~= nil and not same_value(v, row.texts[c]) then
          set[#set + 1] = { column = st.columns[c].name, value = v }
        end
      end
      if #set > 0 then
        changes[#changes + 1] = { kind = "update", key = key_of(row), set = set }
        counts.update = counts.update + 1
        status[original.lnum] = "update"
      end
      for _, p in ipairs(found) do
        if p ~= original then
          insert(p, row)
        end
      end
    end
  end
  for _, p in ipairs(new) do
    insert(p, nil)
  end
  table.sort(errors, function(a, b) return a.lnum < b.lnum end)
  return { changes = changes, errors = errors, counts = counts, status = status }
end

--- "1 update, 2 deletes"
function M.summary(counts)
  local parts = {}
  for _, k in ipairs({ "update", "delete", "insert" }) do
    local n = counts[k]
    if n > 0 then
      parts[#parts + 1] = string.format("%d %s%s", n, k, n == 1 and "" or "s")
    end
  end
  return table.concat(parts, ", ")
end

return M
