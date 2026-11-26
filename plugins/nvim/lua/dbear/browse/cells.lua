--- The text form of a table buffer's lines. Pure functions, no buffers.
---
--- A line is `/<id>:` (the row id, concealed) then the cells separated by `|`:
---
---   /003:42 | Ana | \N | two\nlines
---
--- The `:` keeps word motions (`ciw` on a cell's leading spaces…) from reaching into the id.
---
--- Cell text round-trips: `\\` backslash, `\|` a pipe, `\n` `\r` `\t` and `\xHH` control
--- characters, `\s` a space (only needed at the start or end of a cell, which are otherwise padding).
--- A whole cell `\N` is NULL, `\D` DEFAULT and `\e` the empty string. An empty cell is the empty
--- string in a loaded row and DEFAULT in a new one.

local M = {}

M.SEP = "|"

local escapes = { ["\\"] = "\\\\", ["|"] = "\\|", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }
local unescapes = { ["\\"] = "\\", ["|"] = "|", n = "\n", r = "\r", t = "\t", s = " " }

--- A value's text as cell text (lossless).
function M.escape(s)
  s = s:gsub("[\\|%c]", function(c) return escapes[c] or string.format("\\x%02x", c:byte()) end)
  s = s:gsub("^ +", function(sp) return ("\\s"):rep(#sp) end)
  s = s:gsub(" +$", function(sp) return ("\\s"):rep(#sp) end)
  return s
end

--- Cell text (padding already trimmed) → `{ kind = "text"|"null"|"default"|"empty", text? }`, or nil
--- and a message. "empty" is a blank cell, whose meaning depends on the row (see above).
function M.decode(raw)
  if raw == "" then
    return { kind = "empty" }
  elseif raw == "\\N" then
    return { kind = "null" }
  elseif raw == "\\D" then
    return { kind = "default" }
  elseif raw == "\\e" then
    return { kind = "text", text = "" }
  end
  local out, i, n = {}, 1, #raw
  while i <= n do
    local c = raw:sub(i, i)
    if c == "\\" then
      local e = raw:sub(i + 1, i + 1)
      if e == "x" then
        local hex = raw:sub(i + 2, i + 3)
        if not hex:match("^%x%x$") then
          return nil, "\\x needs two hex digits"
        end
        out[#out + 1] = string.char(tonumber(hex, 16))
        i = i + 4
      elseif unescapes[e] then
        out[#out + 1] = unescapes[e]
        i = i + 2
      elseif e == "N" or e == "D" or e == "e" then
        return nil, "\\" .. e .. " must be the whole cell"
      else
        return nil, e == "" and "a cell can't end with a single \\ (write \\\\)" or ("unknown escape \\" .. e .. " (write \\\\ for a backslash)")
      end
    else
      out[#out + 1] = c
      i = i + 1
    end
  end
  return { kind = "text", text = table.concat(out) }
end

--- The row id prefix: id (a number; 0 for new rows) and the byte length of the prefix. Lines
--- without one return nil, 0. A damaged prefix (`/12x`) returns false.
function M.prefix(line)
  local digits, rest = line:match("^/(%d+)(.?)")
  if not digits then
    return nil, 0
  end
  if rest ~= ":" then
    return false, 0
  end
  return tonumber(digits), #digits + 2
end

--- Byte ranges of the cells after `start` (0-based, end exclusive, without the `|`s), split at
--- unescaped `|`.
function M.ranges(line, start)
  local ranges, from, i, n = {}, start, start + 1, #line
  while i <= n do
    local b = line:byte(i)
    if b == 92 then -- backslash: skip the escaped byte
      i = i + 2
    else
      if b == 124 then
        ranges[#ranges + 1] = { from, i - 1 }
        from = i
      end
      i = i + 1
    end
  end
  ranges[#ranges + 1] = { from, n }
  return ranges
end

local function trim(s)
  return (s:gsub("^ +", ""):gsub(" +$", ""))
end

--- A line → `{ id = n|nil, cells = { trimmed raw text… }, ranges = {…}, plen }`, or nil when the
--- line is blank (and has no id), or false and a message when it can't be read.
function M.parse(line, ncols)
  local id, plen = M.prefix(line)
  if id == false then
    return false, "the row id at the start of the line is damaged"
  end
  local body = line:sub(plen + 1)
  if not id and trim(body) == "" then
    return nil
  end
  if not id and line:match("^[^|]-/%d%d%d+:") then
    return false, "text was typed before the row id (undo it, or start the line with the id)"
  end
  local ranges = M.ranges(line, plen)
  local cells = {}
  for i, r in ipairs(ranges) do
    cells[i] = trim(line:sub(r[1] + 1, r[2]))
  end
  if #cells ~= ncols then
    return false, string.format("%d cells, expected %d", #cells, ncols)
  end
  return { id = id ~= 0 and id or nil, cells = cells, ranges = ranges, plen = plen }
end

local function width(s)
  return vim.api.nvim_strwidth(s)
end
M.width = width

--- Cuts `s` to `max` display cells, ending with "…". Returns the text and whether it was cut.
function M.cut(s, max)
  if not max or max <= 0 or width(s) <= max then
    return s, false
  end
  local out, w = {}, 0
  for _, ch in ipairs(vim.fn.split(s, "\\zs")) do
    local cw = width(ch)
    if w + cw > max - 1 then
      break
    end
    out[#out + 1], w = ch, w + cw
  end
  return table.concat(out) .. "…", true
end

local function pad(s, w, right)
  local gap = string.rep(" ", math.max(0, w - width(s)))
  return right and gap .. s or s .. gap
end

--- A row's line: the id (0: new), then the cells padded to `widths` (numbers on the right).
function M.format(id, cells, widths, numeric, idw)
  local parts = {}
  for i, text in ipairs(cells) do
    local last = i == #cells
    local p = pad(text, widths[i] or 0, numeric[i])
    if last and not numeric[i] then
      p = text
    end
    parts[i] = p
  end
  return string.format("/%0" .. idw .. "d:", id or 0) .. table.concat(parts, " | ")
end

--- The header in the same layout (no id).
function M.header(names, widths, numeric)
  local parts = {}
  for i, name in ipairs(names) do
    parts[i] = pad(name, widths[i], numeric[i])
  end
  return table.concat(parts, " │ ")
end

--- Which cell byte `col` (0-based) of a parsed line is in, and the offset into its trimmed text.
function M.locate(line, parsed, col)
  for i, r in ipairs(parsed.ranges) do
    if col < r[2] or i == #parsed.ranges then
      local lead = #(line:sub(r[1] + 1, r[2]):match("^ *"))
      return i, math.max(0, math.min(col - r[1] - lead, #parsed.cells[i]))
    end
  end
  return 1, 0
end

--- The byte column of offset `off` into cell `i`'s trimmed text.
function M.position(line, parsed, i, off)
  local r = parsed.ranges[i]
  local lead = #(line:sub(r[1] + 1, r[2]):match("^ *"))
  return r[1] + lead + math.min(off, #parsed.cells[i])
end

return M
