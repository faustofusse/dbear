--- Progress of the mirror's listings, through what the running Neovim has (no hard dependency):
---
---   1. fidget.nvim's progress API (`fidget.progress.handle`), when it's installed;
---   2. Neovim's progress messages (`nvim_echo` with `kind = "progress"`, Neovim 0.12+);
---   3. else one `vim.notify` (INFO) when a listing starts, nothing when it ends.
---
--- Nothing is shown for a listing that ends within `M.delay` ms (the root's `connections.list`
--- reads the local store), and every item shown is ended (success or failure) when its listing
--- ends, however it ends: there's always a `finish` for a `start` (see `sync.once`).

local M = {}

--- How long a listing runs before it's shown (ms).
M.delay = 250
--- Forces a backend ("fidget", "echo", "notify"), for tests.
M.force = nil

--- Listings in progress: key → { timer, label, message, shown, fidget | echo_id | notified }.
local items = {}
local echo_ids = 0

--- Which backend this Neovim has: "fidget", "echo" or "notify".
function M.backend()
  if M.force then
    return M.force
  end
  local ok, fidget = pcall(require, "fidget.progress")
  if ok and type(fidget) == "table" and type(fidget.handle) == "table" and type(fidget.handle.create) == "function" then
    return "fidget"
  end
  -- Progress messages came with the `Progress` event (Neovim 0.12).
  if vim.fn.exists("##Progress") == 1 then
    return "echo"
  end
  return "notify"
end

local function echo(it, text, status)
  local ok, id = pcall(vim.api.nvim_echo, { { text } }, false, {
    id = it.echo_id,
    kind = "progress",
    source = "dbear",
    title = "dbear",
    status = status,
  })
  return ok and id or nil
end

local function show(it)
  local backend = M.backend()
  if backend == "fidget" then
    local ok, handle = pcall(function()
      return require("fidget.progress").handle.create({
        title = "mirror",
        message = it.message,
        lsp_client = { name = "dbear" },
      })
    end)
    if ok and handle then
      it.fidget = handle
      return
    end
    backend = "echo" -- fidget failed: fall through
  end
  if backend == "echo" and vim.fn.exists("##Progress") == 1 then
    echo_ids = echo_ids + 1
    it.echo_id = "dbear.tree.progress." .. echo_ids
    if echo(it, it.message, "running") then
      return
    end
    it.echo_id = nil
  end
  it.notified = true
  vim.notify("dbear: " .. it.message, vim.log.levels.INFO)
end

--- A listing of `key` started: shown after `M.delay` ms unless it ended by then. `label` is what
--- is listed ("Staging", "databases of Staging", "Staging / app").
function M.start(key, label)
  M.finish(key) -- a key is listed once at a time; never leave an older item behind
  local it = { label = label, message = "listing " .. label .. "…" }
  items[key] = it
  it.timer = vim.defer_fn(function()
    it.timer = nil
    if items[key] == it then
      it.shown = true
      show(it)
    end
  end, M.delay)
end

--- A listing ended (`err`: it failed). Ends what was shown; cancels what wasn't yet.
function M.finish(key, err)
  local it = items[key]
  if not it then
    return
  end
  items[key] = nil
  if it.timer then
    pcall(function() it.timer:stop() it.timer:close() end)
    it.timer = nil
  end
  if not it.shown then
    return
  end
  local text = err and ("couldn't list " .. it.label) or ("listed " .. it.label)
  if it.fidget then
    pcall(function()
      it.fidget:report({ message = text })
      it.fidget:finish()
    end)
  elseif it.echo_id then
    echo(it, text, err and "failed" or "success")
  end
  -- notify: nothing (no "done" message).
end

--- Whether anything is shown or waiting to be (tests).
function M.active()
  return next(items) ~= nil
end

return M
