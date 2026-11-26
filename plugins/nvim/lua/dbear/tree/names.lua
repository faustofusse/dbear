--- File names in the mirror: a readable, reversible escaping of database names, and unique names
--- among siblings on case-insensitive file systems.
---
--- `escape` keeps a name as it is except for what a file name can't hold or would hide:
---   `%` → `%25`, `/` `\` `:` `*` `?` `"` `<` `>` `|` and control characters → `%XX`,
---   a leading `.` or space and a trailing `.` or space → `%XX`, bytes that aren't UTF-8 → `%XX`,
---   the empty name → `%-`.
--- Two more markers never come from a name (a `%` in a name is always `%25`):
---   `%~N` (N ≥ 2) at the end: the Nth of sibling names that the file system can't tell apart
---   (`Users` and `users` on macOS), in their listing order;
---   `%+<hash>` at the end: a name too long for a file name, cut (not reversible).
--- `unescape` reverses it. The mirror never relies on it: each entry's exact target is in the
--- manifest and in the table file's header.

local M = {}

--- File name components above this many bytes are cut (file systems allow 255).
M.MAX = 200

local reserved = {}
for c in ([[%/\:*?"<>|]]):gmatch(".") do
  reserved[c:byte()] = true
end

--- Length of the valid UTF-8 sequence at `i`, or nil.
local function utf8_len(s, i)
  local c = s:byte(i)
  local n
  if c < 0x80 then
    return 1
  elseif c >= 0xC2 and c <= 0xDF then
    n = 2
  elseif c >= 0xE0 and c <= 0xEF then
    n = 3
  elseif c >= 0xF0 and c <= 0xF4 then
    n = 4
  else
    return nil
  end
  for k = 1, n - 1 do
    local d = s:byte(i + k)
    if not d or d < 0x80 or d > 0xBF then
      return nil
    end
  end
  local d = s:byte(i + 1)
  if (c == 0xE0 and d < 0xA0) or (c == 0xED and d >= 0xA0) or (c == 0xF0 and d < 0x90) or (c == 0xF4 and d >= 0x90) then
    return nil -- overlong, surrogate, above U+10FFFF
  end
  return n
end

local function hex(b)
  return string.format("%%%02X", b)
end

--- The escaped name, cut to `M.MAX` bytes when needed (then not reversible).
function M.escape(name)
  if name == "" then
    return "%-"
  end
  local out, i, n = {}, 1, #name
  while i <= n do
    local b = name:byte(i)
    local len = utf8_len(name, i)
    if not len then
      out[#out + 1] = hex(b)
      i = i + 1
    elseif len == 1 then
      local edge = (i == 1 and (b == 0x2E or b == 0x20)) or (i == n and (b == 0x2E or b == 0x20))
      out[#out + 1] = (b < 0x20 or b == 0x7F or reserved[b] or edge) and hex(b) or string.char(b)
      i = i + 1
    else
      out[#out + 1] = name:sub(i, i + len - 1)
      i = i + len
    end
  end
  local s = table.concat(out)
  if #s <= M.MAX then
    return s
  end
  -- Cut on a piece boundary (never inside `%XX` or a UTF-8 sequence), then mark it with a hash.
  local cut, used = {}, 0
  for _, piece in ipairs(out) do
    if used + #piece > M.MAX - 12 then
      break
    end
    cut[#cut + 1] = piece
    used = used + #piece
  end
  return table.concat(cut) .. "%+" .. vim.fn.sha256(name):sub(1, 10)
end

--- The name `escape` came from (without the `%~N` marker), or nil when it was cut or isn't one.
function M.unescape(s)
  s = s:gsub("%%~%d+$", "")
  if s == "%-" then
    return ""
  end
  if s:find("%+", 1, true) then
    return nil
  end
  local bad = false
  local name = s:gsub("%%(..)", function(h)
    if not h:match("^%x%x$") then
      bad = true
      return ""
    end
    return string.char(tonumber(h, 16))
  end)
  return not bad and name or nil
end

--- The name a folder name typed by someone stands for (a connection folder renamed in oil or with
--- `mv`): like `unescape`, but lenient with what people type. `%XX` escapes are reversed, any
--- other `%` is kept as typed (`100%`, `50%off`), a trailing `%~N` (the mirror's collision
--- marker, left in place) is dropped and `%-` is the empty name. Returns nil and why for a name
--- the mirror cut (`%+<hash>`): it can't tell what the rest was.
function M.parse(file)
  local s = file:gsub("%%~%d+$", "")
  if s == "%-" then
    return ""
  end
  if s:find("%%%+%x+$") then
    return nil, "the folder's name was cut to fit (it ends in %+…): type the whole name"
  end
  return (s:gsub("%%(%x%x)", function(h) return string.char(tonumber(h, 16)) end))
end

--- The key two names collide under on a case-insensitive file system.
function M.fold(s)
  return vim.fn.tolower(s)
end

--- Gives each item (`{ name, ext? }`, in order) a `file` name unique among them even where case
--- doesn't count: the first keeps its plain name, the next ones get `%~2`, `%~3`… `taken` (folded
--- names) are already used.
function M.assign(items, taken)
  taken = taken or {}
  for _, item in ipairs(items) do
    local base, ext = M.escape(item.name), item.ext or ""
    local file, k = base .. ext, 1
    while taken[M.fold(file)] do
      k = k + 1
      file = base .. "%~" .. k .. ext
    end
    taken[M.fold(file)] = true
    item.file = file
  end
  return items
end

--- `file` with a `%~N` marker for the next free N among the folded names in `taken`.
function M.bump(file, ext, taken)
  local base = file:sub(1, #file - #ext):gsub("%%~%d+$", "")
  local k = 1
  local candidate = file
  while taken[M.fold(candidate)] do
    k = k + 1
    candidate = base .. "%~" .. k .. ext
  end
  return candidate
end

return M
