--- Every file system change the mirror makes, each one guarded against its root: nothing outside it
--- (or the root itself, except its permissions) is ever written, chmod-ed, renamed or deleted.
---
--- A path passes the guard when it's absolute, has no `.`/`..` component, is strictly below the
--- root, and its parent directory, with symlinks resolved, is still the root or below it. A symlink
--- inside the mirror (the mirror never makes one) is only ever unlinked: never followed, written
--- through or chmod-ed. Functions raise an error (`dbear mirror: …`) when the guard refuses.

local uv = vim.uv or vim.loop

local M = {}

--- The manifest's name, at the root.
M.MANIFEST = ".dbear-mirror.json"

M.DIR_MODE = tonumber("555", 8) -- r-x: oil (or anything) can't add, delete or rename entries
M.FILE_MODE = tonumber("444", 8)
local WRITABLE_DIR = tonumber("755", 8)
local WRITABLE_FILE = tonumber("644", 8)

--- Table files open in a rows buffer → true. They stay writable while open: Vim refuses `:w`
--- (E505) for a file it can't write, even when BufWriteCmd saves something else (the rows).
local unlocked = {}

local root, real -- as configured (absolute, normalized) and with symlinks resolved

local function normalize(path)
  path = vim.fs.normalize(path)
  if #path > 1 then
    path = path:gsub("/+$", "")
  end
  return path
end
M.normalize = normalize

--- `path` with symlinks resolved, also when it doesn't exist yet (through its nearest existing
--- ancestor), so buffer names under /private/tmp match a root under /tmp before it's created.
local function resolve(path)
  local rest = ""
  local p = path
  while p and p ~= "" do
    local r = uv.fs_realpath(p)
    if r then
      return normalize(r .. rest)
    end
    local parent = vim.fs.dirname(p)
    if parent == p then
      break
    end
    rest = "/" .. vim.fs.basename(p) .. rest
    p = parent
  end
  return nil
end

function M.set_root(path)
  root = normalize(vim.fn.fnamemodify(vim.fn.expand(path), ":p"))
  real = resolve(root)
end

function M.root()
  return root
end

--- The root's realpath, once it exists.
function M.real_root()
  if not real then
    real = resolve(root)
  end
  return real
end

local function below(path, base)
  return base ~= nil and #path > #base + 1 and path:sub(1, #base + 1) == base .. "/"
end

--- `path` (a buffer name, maybe under the root's realpath) as a path under the configured root,
--- or nil when it isn't the root or below it.
function M.inside(path)
  if not path or path == "" then
    return nil
  end
  path = normalize(path)
  if path == root or below(path, root) then
    return path
  end
  local r = M.real_root()
  if r and r ~= root and (path == r or below(path, r)) then
    return root .. path:sub(#r + 1)
  end
  return nil
end

--- The path relative to the root ("" for the root), or nil.
function M.rel(path)
  path = M.inside(path)
  if not path then
    return nil
  end
  return path == root and "" or path:sub(#root + 2)
end

local function refuse(what, path)
  error("dbear mirror: refusing to " .. what .. " " .. tostring(path) .. " (outside the mirror root " .. tostring(root) .. ")", 0)
end

--- Raises unless `path` is strictly below the root (see the module's doc).
function M.check(path, what)
  if type(path) ~= "string" or not root or root == "/" or root == "" then
    refuse(what or "touch", path)
  end
  local absolute = path:sub(1, 1) == "/" or path:match("^%a:/") ~= nil
  if not absolute or path ~= normalize(path) then
    refuse(what or "touch", path)
  end
  for part in path:gmatch("[^/]+") do
    if part == "." or part == ".." then
      refuse(what or "touch", path)
    end
  end
  if not below(path, root) then
    refuse(what or "touch", path)
  end
  local parent = uv.fs_realpath(vim.fs.dirname(path))
  local r = M.real_root()
  if not parent or not r then
    refuse(what or "touch", path)
  end
  parent = normalize(parent)
  if parent ~= r and not below(parent, r) then
    refuse(what or "touch", path)
  end
  return path
end

--- `lstat` type: "file", "directory", "link", … or nil when missing.
function M.kind(path)
  local st = uv.fs_lstat(path)
  return st and st.type or nil
end

local function ok(res, err, what, path)
  if type(res) == "table" then -- { uv call's results… }
    res, err = res[1], res[2]
  end
  if not res then
    error("dbear mirror: can't " .. what .. " " .. path .. ": " .. tostring(err), 0)
  end
  return res
end

--- chmod, never through a symlink. The root itself may be chmod-ed too.
function M.chmod(path, mode)
  if path ~= root then
    M.check(path, "chmod")
  end
  local kind = M.kind(path)
  if kind == "file" or kind == "directory" then
    ok({ uv.fs_chmod(path, mode) }, nil, "chmod", path)
  end
end

--- Creates the root (and its parents) when missing. Refuses a folder that isn't empty and has no
--- manifest: it isn't a mirror, and sync deletes what it doesn't know.
function M.ensure_root()
  local kind = uv.fs_stat(root)
  if not kind then
    ok(vim.fn.mkdir(root, "p") == 1, "mkdir failed", "create", root)
  elseif kind.type ~= "directory" then
    error("dbear mirror: " .. root .. " isn't a folder", 0)
  else
    local has_manifest = uv.fs_lstat(root .. "/" .. M.MANIFEST) ~= nil
    if not has_manifest then
      local handle = uv.fs_scandir(root)
      if handle and uv.fs_scandir_next(handle) then
        error("dbear mirror: " .. root .. " isn't empty and isn't a dbear mirror (no " .. M.MANIFEST .. "): pick an empty or new folder", 0)
      end
    end
  end
  real = uv.fs_realpath(root) and normalize(uv.fs_realpath(root)) or resolve(root)
end

--- Runs `fn` with `dir` (the root or a folder below it) writable, then makes it read-only again.
function M.with_writable(dir, fn)
  M.chmod(dir, WRITABLE_DIR)
  local res = { pcall(fn) }
  pcall(M.chmod, dir, M.DIR_MODE)
  if not res[1] then
    error(res[2], 0)
  end
  return unpack(res, 2)
end

--- Creates folder `path` (its parent must be writable). Nothing when it's already one; a file or
--- symlink in its place is removed first.
function M.mkdir(path)
  M.check(path, "create")
  local kind = M.kind(path)
  if kind == "directory" then
    return false
  end
  if kind then
    M.remove(path)
  end
  ok({ uv.fs_mkdir(path, WRITABLE_DIR) }, nil, "create", path)
  return true
end

local function read(path)
  local fd = uv.fs_open(path, "r", 0)
  if not fd then
    return nil
  end
  local st = uv.fs_fstat(fd)
  local data = st and uv.fs_read(fd, st.size, 0)
  uv.fs_close(fd)
  return data
end
M.read = read

--- Writes file `path` read-only (its folder must be writable) unless it already holds `content`.
--- A new file is created exclusively, so a symlink is never written through.
function M.write(path, content, force)
  M.check(path, "write")
  local kind = M.kind(path)
  local mode = unlocked[path] and WRITABLE_FILE or M.FILE_MODE
  if kind == "file" and not force and read(path) == content then
    M.chmod(path, mode)
    return false
  end
  if kind then
    M.remove(path)
  end
  local fd = ok({ uv.fs_open(path, "wx", mode) }, nil, "write", path)
  local written, err = uv.fs_write(fd, content, 0)
  uv.fs_close(fd)
  ok(written, err, "write", path)
  return true
end

--- Removes `path`: a file or a symlink is unlinked, a folder emptied (made writable first) and
--- removed. Never follows a symlink.
function M.remove(path)
  M.check(path, "delete")
  local kind = M.kind(path)
  if not kind then
    return
  end
  if kind ~= "directory" then
    ok({ uv.fs_unlink(path) }, nil, "delete", path)
    return
  end
  ok({ uv.fs_chmod(path, WRITABLE_DIR) }, nil, "chmod", path)
  for name in M.list(path) do
    M.remove(path .. "/" .. name)
  end
  ok({ uv.fs_rmdir(path) }, nil, "delete", path)
end

--- Lets a table file open in a rows buffer be "written" (`on`), or makes it read-only again.
function M.unlock(path, on)
  M.check(path, "chmod")
  unlocked[path] = on or nil
  if M.kind(path) == "file" then
    M.chmod(path, on and WRITABLE_FILE or M.FILE_MODE)
  end
end

--- Renames `from` to `to` (both below the root; their folder must be writable). A folder is made
--- writable for the move (some systems require it) and read-only again after.
function M.rename(from, to)
  M.check(from, "rename")
  M.check(to, "rename to")
  local dir = M.kind(from) == "directory"
  if dir then
    M.chmod(from, WRITABLE_DIR)
  end
  ok({ uv.fs_rename(from, to) }, nil, "rename", from)
  if dir then
    M.chmod(to, M.DIR_MODE)
  end
end

--- Iterates over the names in folder `path` (none when it can't be read).
function M.list(path)
  local handle = uv.fs_scandir(path)
  local names = {}
  while handle do
    local name = uv.fs_scandir_next(handle)
    if not name then
      break
    end
    names[#names + 1] = name
  end
  local i = 0
  return function()
    i = i + 1
    return names[i]
  end
end

--- The inode of `path` (no symlink followed), or nil.
function M.inode(path)
  local st = uv.fs_lstat(path)
  return st and st.ino or nil
end

--- Makes sure folder `path` (the root or below) exists, creating it and its parents read-only.
function M.ensure_dir(path)
  if path == root then
    return M.ensure_root()
  end
  if M.kind(path) == "directory" then
    return
  end
  local parent = vim.fs.dirname(path)
  M.ensure_dir(parent)
  M.with_writable(parent, function()
    M.mkdir(path)
    M.chmod(path, M.DIR_MODE)
  end)
end

--- Writes the root's manifest (through a temp file and a rename).
function M.write_manifest(content)
  M.with_writable(root, function()
    local tmp = root .. "/" .. M.MANIFEST .. ".tmp"
    M.write(tmp, content, true)
    M.rename(tmp, root .. "/" .. M.MANIFEST)
  end)
end

return M
