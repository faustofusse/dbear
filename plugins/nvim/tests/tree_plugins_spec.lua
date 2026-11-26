-- The mirror with the real oil.nvim and telescope.nvim (+ plenary), when they're installed: run by
-- tests/run.sh with DBEAR_TEST_PLUGINS (a folder holding oil.nvim, telescope.nvim and
-- plenary.nvim, used read-only) against the same store as tree_spec.lua, in a mirror of its own.
-- oil gets the options the plugin is meant to live with: skip_confirm_for_simple_edits,
-- watch_for_changes, show_hidden, signcolumn = "yes".
local mirror = assert(os.getenv("DBEAR_TEST_MIRROR"))
local plugins = assert(os.getenv("DBEAR_TEST_PLUGINS"))
for _, p in ipairs({ "plenary.nvim", "telescope.nvim", "oil.nvim" }) do
  vim.opt.rtp:append(plugins .. "/" .. p)
end
vim.cmd("runtime plugin/dbear.lua")
vim.cmd("runtime! plugin/telescope.lua plugin/oil.lua")
require("oil").setup({
  skip_confirm_for_simple_edits = true,
  watch_for_changes = true,
  view_options = { show_hidden = true },
  win_options = { signcolumn = "yes" },
})
require("telescope").setup({})
local dbear = require("dbear")
dbear.setup({ bin = os.getenv("DBEAR_TEST_BIN"), tree = { root = mirror } })
local tree = require("dbear.tree")
local fs = require("dbear.tree.fs")
local grid = require("dbear.browse.grid")
local uv = vim.uv

local failures, passed = 0, 0
local function print(s)
  io.stdout:write(s, "\n")
end
local messages = {}
vim.notify = function(msg, level)
  messages[#messages + 1] = { msg = msg, level = level }
end

local function test(name, fn)
  messages = {}
  local ok, err = pcall(fn)
  if ok then
    passed = passed + 1
    print("ok   " .. name)
  else
    failures = failures + 1
    print("FAIL " .. name .. "\n     " .. tostring(err))
    for _, m in ipairs(messages) do
      print("     notify: " .. m.msg)
    end
  end
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_is_valid(w) and vim.api.nvim_win_get_config(w).relative ~= "" then
      pcall(vim.api.nvim_win_close, w, true)
    end
  end
  vim.cmd("stopinsert")
  vim.cmd("silent! only")
  vim.cmd("enew!")
end

local function eq(want, got, what)
  if not vim.deep_equal(want, got) then
    error(string.format("%s:\nwant %s\n got %s", what or "value", vim.inspect(want), vim.inspect(got)), 2)
  end
end

local function wait(what, check, ms)
  if not vim.wait(ms or 10000, check, 20) then
    error("timed out waiting for " .. what, 2)
  end
end

local function keys(k)
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(k, true, false, true), "mx", false)
end

local function bufname()
  local name = vim.api.nvim_buf_get_name(0)
  local oil = name:match("^oil://(.*)$")
  if oil then
    return "oil://" .. (fs.inside(oil) or oil) .. (oil:sub(-1) == "/" and "/" or "")
  end
  return fs.inside(name) or name
end

local function lines()
  return vim.api.nvim_buf_get_lines(0, 0, -1, false)
end

--- The oil buffer's entry names (as shown).
local function entries()
  local out = {}
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e then
      out[#out + 1] = e.name
    end
  end
  table.sort(out)
  return out
end

local function oil_ready()
  return vim.bo.filetype == "oil" and not vim.b.oil_rendering
end

--- In an oil buffer: the cursor on `name`, then <CR> (oil's select).
local function select(name)
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e and e.name == name then
      vim.api.nvim_win_set_cursor(0, { i, 0 })
      keys("<CR>")
      return
    end
  end
  error("no " .. name .. " in " .. vim.inspect(entries()))
end

local function state()
  return grid.states[vim.api.nvim_get_current_buf()]
end

local function run(conn, sql)
  local err, res = dbear.rpc():request_sync("query.run", { connection = conn, sql = sql }, 10000)
  assert(not err, vim.inspect(err))
  return res.result.ok.rows
end

test("oil: root → connection (listed on entering) → table → rows → :w → - back", function()
  vim.cmd("DbearOpen")
  wait("oil root", function() return oil_ready() and vim.tbl_contains(entries(), "Shop") end)
  eq("oil://" .. mirror .. "/", bufname(), "the root in oil")
  select("Shop")
  -- Listed lazily: oil shows it once done, without :e.
  wait("shop.db listed", function() return oil_ready() and vim.tbl_contains(entries(), "shop.db") end)
  eq("oil://" .. mirror .. "/Shop/", bufname(), "in Shop")
  select("shop.db")
  wait("main", function() return oil_ready() and vim.tbl_contains(entries(), "main") end)
  select("main")
  wait("tables", function() return oil_ready() and vim.tbl_contains(entries(), "users.table") end)
  select("users.table")
  wait("rows", function() return state() and state().ready end)
  eq(mirror .. "/Shop/shop.db/main/users.table", bufname(), "the file")
  local st = state()
  local l = lines()[2]
  vim.api.nvim_win_set_cursor(0, { 2, l:find("Bo", 1, true) - 1 })
  keys("ciwBob<Esc>")
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  vim.cmd("write")
  wait("confirmation", function() return st.confirm_win ~= nil end)
  keys("y")
  wait("saved", function() return not st.saving and not st.loading end)
  eq({ { "Bob" } }, run("shop", "select name from users where id = 2"), "saved in the database")
  keys("-")
  wait("oil folder", function() return oil_ready() and vim.tbl_contains(entries(), "users.table") end)
  eq("oil://" .. mirror .. "/Shop/shop.db/main/", bufname(), "back in oil, in the table's folder")
end)

test("oil: deleting or renaming a table line fails harmlessly, the mirror stays", function()
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/Shop/shop.db/main"))
  wait("oil", function() return oil_ready() and vim.tbl_contains(entries(), "users.table") end)
  local before = entries()
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e and e.name == "orders.table" then
      vim.api.nvim_win_set_cursor(0, { i, 0 })
    end
  end
  -- oil sees the folder is read-only and makes its buffer nomodifiable: `dd` does nothing.
  eq(false, vim.bo.modifiable, "oil: nomodifiable")
  pcall(keys, "dd")
  eq(before, entries(), "nothing deleted in the buffer")
  eq(false, vim.bo.modified, "not modified")
  -- Forced anyway: oil asks about the delete, it fails (permission denied), nothing changes.
  vim.bo.modifiable = true
  keys("dd")
  vim.cmd("write")
  wait("oil's confirmation", function() return vim.api.nvim_win_get_config(0).relative ~= "" end)
  keys("y")
  local function denied()
    for _, m in ipairs(messages) do
      if m.msg:lower():find("permission denied", 1, true) then
        return true
      end
    end
  end
  wait("oil gave up", function() return not require("oil.mutator").is_mutating() and denied() end)
  assert(uv.fs_lstat(mirror .. "/Shop/shop.db/main/orders.table"), "the file is still there")
  -- A rename is a "simple edit" (no confirmation with skip_confirm_for_simple_edits): it fails too.
  vim.cmd("edit!")
  wait("oil", function() return oil_ready() and #entries() > 0 end)
  messages = {}
  vim.bo.modifiable = true
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e and e.name == "users.table" then
      local l = vim.api.nvim_buf_get_lines(0, i - 1, i, false)[1]
      vim.api.nvim_buf_set_lines(0, i - 1, i, false, { (l:gsub("users%.table", "people.table")) })
    end
  end
  vim.cmd("write")
  wait("oil gave up", function() return not require("oil.mutator").is_mutating() and denied() end)
  assert(uv.fs_lstat(mirror .. "/Shop/shop.db/main/users.table"), "not renamed")
  eq(nil, uv.fs_lstat(mirror .. "/Shop/shop.db/main/people.table"), "no new name")
  eq({ { 1 } }, run("shop", "select count(*) from sqlite_master where name = 'orders'"), "the table is still in the database")
  -- After a refresh, oil shows the mirror as it is.
  vim.cmd("edit!")
  wait("oil", function() return oil_ready() and #entries() > 0 end)
  eq(before, entries(), "intact")
end)

test("telescope find_files on the root finds a table and opens its rows", function()
  vim.cmd("DbearFind")
  local prompt
  wait("picker", function()
    prompt = vim.bo.filetype == "TelescopePrompt" and vim.api.nvim_get_current_buf() or nil
    return prompt
  end)
  local action_state = require("telescope.actions.state")
  keys("users.table")
  wait("results", function()
    local e = action_state.get_selected_entry()
    return e and (e.value or e[1]) == "Shop/shop.db/main/users.table"
  end, 5000)
  require("telescope.actions").select_default(prompt)
  wait("rows", function() return state() and state().ready end, 10000)
  eq(mirror .. "/Shop/shop.db/main/users.table", bufname(), "opened")
  eq("dbear_table", vim.bo.filetype, "rows buffer")
end)

test("telescope live_grep on the root finds a column and opens the rows", function()
  vim.cmd("DbearGrep")
  local prompt
  wait("picker", function()
    prompt = vim.bo.filetype == "TelescopePrompt" and vim.api.nvim_get_current_buf() or nil
    return prompt
  end)
  local action_state = require("telescope.actions.state")
  keys("email  text")
  wait("results", function()
    local e = action_state.get_selected_entry()
    return e and e.filename and e.filename:find("users.table", 1, true)
  end)
  local e = action_state.get_selected_entry()
  assert(e.filename:find("Shop/shop.db/main/users.table", 1, true), e.filename)
  require("telescope.actions").select_default(prompt)
  wait("rows", function() return state() and state().ready end, 10000)
  eq(mirror .. "/Shop/shop.db/main/users.table", bufname(), "opened")
end)

-- MARK: renaming connections from oil

local manifest = require("dbear.tree.manifest")

local function ls(rel)
  local out = {}
  for name in fs.list(mirror .. (rel and rel ~= "" and "/" .. rel or "")) do
    out[#out + 1] = name
  end
  table.sort(out)
  return out
end

local function mode(rel)
  return string.format("%o", bit.band(uv.fs_lstat(mirror .. (rel ~= "" and "/" .. rel or "")).mode, tonumber("777", 8)))
end

--- The saved connections as `id=name`, from the store (connections.list).
local function saved()
  local err, res = dbear.rpc():request_sync("connections.list", nil, 10000)
  assert(not err, vim.inspect(err))
  local out = {}
  for _, c in ipairs(res.connections) do
    out[#out + 1] = c.id .. "=" .. c.name
  end
  return out
end

--- `dbear-cli ls` in a process of its own (no server): the names, from the store file.
local function cli_names()
  local out = vim.system({ os.getenv("DBEAR_TEST_BIN"), "--no-server", "ls", "--format", "tsv" }, { text = true }):wait()
  assert(out.code == 0, out.stderr)
  return out.stdout
end

local function said(pattern)
  for _, m in ipairs(messages) do
    if m.msg:find(pattern, 1, true) then
      return true
    end
  end
  return false
end

local function idle()
  return tree.idle() and not require("oil.mutator").is_mutating()
end

--- The oil root, rendered and listed.
local function oil_root()
  vim.cmd("DbearOpen")
  wait("oil root", function() return oil_ready() and #entries() > 0 and idle() end)
  if vim.bo.modified then -- left modified by an earlier test
    vim.cmd("edit!")
    wait("oil root", function() return oil_ready() and #entries() > 0 and idle() end)
  end
  wait("modifiable", function() return vim.bo.modifiable end)
end

--- In the current oil buffer: the line of entry `name` (a folder) now says `new`.
local function rename_line(name, new)
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e and e.name == name then
      local l = vim.api.nvim_buf_get_lines(0, i - 1, i, false)[1]
      local at = l:find(name .. "/", 1, true)
      vim.api.nvim_buf_set_lines(0, i - 1, i, false, { l:sub(1, at - 1) .. new .. "/" .. l:sub(at + #name + 1) })
      return
    end
  end
  error("no " .. name .. " in " .. vim.inspect(entries()))
end

--- `:w` in oil, then waits for oil, the listing that follows and oil's redraw.
local function oil_write(confirm, parse_error)
  vim.cmd("write")
  if parse_error then
    vim.wait(300)
    assert(said("Error parsing oil buffers"), "oil refuses: " .. vim.inspect(messages))
    return
  end
  if confirm then
    wait("oil's confirmation", function() return vim.api.nvim_win_get_config(0).relative ~= "" end)
    keys("y")
  end
  vim.wait(300)
  if said("Error parsing oil buffers") then
    local d = vim.tbl_map(function(x) return (x.lnum + 1) .. ": " .. x.message end, vim.diagnostic.get(0))
    error("oil couldn't parse: " .. vim.inspect(d) .. "\n" .. table.concat(lines(), "\n"))
  end
  wait("done", function() return idle() and oil_ready() end, 20000)
end

local function ident(rel)
  return fs.identity(mirror .. "/" .. rel)
end

test("oil: renaming a connection folder renames the connection; nothing else changes", function()
  oil_root()
  eq("555", mode(""), "the root is read-only on disk")
  eq(true, vim.bo.modifiable, "but its oil buffer can be edited")
  local before_store = saved()
  local shop = ident("Shop")
  rename_line("Shop", "Store")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=Store") end)
  wait("oil redrawn", function() return vim.tbl_contains(entries(), "Store") end)
  assert(said("dbear: renamed “Shop” to “Store”"), vim.inspect(messages))
  eq(vim.tbl_map(function(x) return x == "shop=Shop" and "shop=Store" or x end, before_store), saved(), "only that name changed")
  assert(("\n" .. cli_names()):find("[\t\n]Store[\t\n]"), "dbear-cli ls sees it: " .. cli_names())
  eq(false, vim.tbl_contains(ls(""), "Shop"), "no Shop folder")
  eq(shop, ident("Store"), "the same folder, contents and all")
  assert(uv.fs_lstat(mirror .. "/Store/shop.db/main/users.table"), "contents kept")
  eq("shop", manifest.lookup("Store").conn.id, "manifest")
  eq("555", mode(""), "root read-only again")
  eq("555", mode("Store"), "folder read-only again")
  -- A second listing changes nothing.
  local listing = ls("")
  messages = {}
  local done
  require("dbear.tree.sync").root(nil, function(e) done = { e } end)
  wait("listed", function() return done end)
  eq(nil, done[1], "no error")
  eq(listing, ls(""), "same folders")
  eq(shop, ident("Store"), "same folder")
  eq({}, messages, "nothing to say")
end)

test("oil: odd characters, a case-only change, a left-over %~N and a duplicate name", function()
  -- Now: Store (id shop), shop (id other: no collision left, so no number), Staging, Replica…
  oil_root()
  local function fresh(name)
    vim.cmd("edit!")
    wait("oil", function() return oil_ready() and vim.tbl_contains(entries(), name) and idle() end)
    messages = {}
  end
  rename_line("Store", "a%2Fb 100%")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=a/b 100%") end)
  wait("canonical folder", function() return idle() and vim.tbl_contains(ls(""), "a%2Fb 100%25") end)
  eq("shop", manifest.lookup("a%2Fb 100%25").conn.id, "escaped back")
  -- Case only (the same folder on a case-insensitive file system).
  fresh("a%2Fb 100%25")
  local same = ident("a%2Fb 100%25")
  rename_line("a%2Fb 100%25", "A%2FB 100%25")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=A/B 100%") end)
  wait("folder", function() return idle() and vim.tbl_contains(ls(""), "A%2FB 100%25") end)
  eq(same, ident("A%2FB 100%25"), "the same folder")
  -- A name that only differs by case from another connection's, with a %~N typed (dropped): the
  -- first in the user's order keeps the plain name, the other folder gets the number.
  fresh("A%2FB 100%25")
  rename_line("A%2FB 100%25", "SHOP%~2")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=SHOP") end)
  wait("folders", function() return idle() and manifest.lookup("SHOP") and manifest.lookup("SHOP").conn.id == "shop" end)
  eq("other", manifest.lookup("shop%~2").conn.id, "the other one is numbered")
  assert(said("renamed “A/B 100%” to “SHOP”"), vim.inspect(messages))
  -- Exactly the name of another folder: oil refuses it itself (duplicate file name).
  fresh("SHOP")
  local before, store_before = ls(""), saved()
  rename_line("SHOP", "Replica")
  oil_write(false, true)
  eq(store_before, saved(), "store unchanged")
  eq(before, ls(""), "folders unchanged")
  -- The same name through a numbered folder name: allowed (the store allows duplicates). "SHOP"
  -- (id shop) comes before Replica (id prod-replica) in the user's order: it takes the plain name.
  fresh("SHOP")
  rename_line("SHOP", "Replica%~2")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=Replica") end)
  wait("folders", function() return idle() and manifest.lookup("Replica") and manifest.lookup("Replica").conn.id == "shop" end)
  eq("prod-replica", manifest.lookup("Replica%~2").conn.id, "the later one is numbered")
  -- A cut name (…%+hash) can't be read back: refused, put back.
  fresh("Replica")
  rename_line("Replica", "abc%+0123456789")
  oil_write()
  wait("refused", function() return said("was cut") end)
  wait("put back", function() return idle() and manifest.lookup("Replica") and manifest.lookup("Replica").conn.id == "shop" end)
  eq(nil, vim.tbl_contains(ls(""), "abc%+0123456789") or nil, "no such folder")
  -- And back to Shop for the next tests (via :DbearRename, cursor on the folder).
  fresh("Replica")
  for i = 1, vim.api.nvim_buf_line_count(0) do
    local e = require("oil").get_entry_on_line(0, i)
    if e and e.name == "Replica" then
      vim.api.nvim_win_set_cursor(0, { i, 0 })
    end
  end
  vim.cmd("DbearRename Shop")
  wait("the store", function() return vim.tbl_contains(saved(), "shop=Shop") end)
  wait("folders", function() return idle() and manifest.lookup("Shop") and manifest.lookup("Replica") end)
  eq("prod-replica", manifest.lookup("Replica").conn.id, "Replica's folder is plain again")
  eq("other", manifest.lookup("shop%~2").conn.id, "shop is numbered again")
  assert(uv.fs_lstat(mirror .. "/Shop/shop.db/main/users.table"), "contents followed every rename")
end)

test("oil: deleting, creating, copying and moving at the root fail; the store is unchanged", function()
  oil_root()
  local before, store_before = ls(""), saved()
  local function unchanged(what)
    eq(store_before, saved(), what .. ": store unchanged")
    wait(what .. ": folders unchanged", function() return idle() and vim.deep_equal(before, ls("")) end)
    eq("555", mode(""), what .. ": root read-only")
  end
  local function fresh()
    vim.cmd("edit " .. vim.fn.fnameescape(mirror))
    vim.cmd("edit!")
    wait("oil", function() return oil_ready() and #entries() > 0 and idle() end)
    messages = {}
  end
  local function line_of(name)
    for i = 1, vim.api.nvim_buf_line_count(0) do
      local e = require("oil").get_entry_on_line(0, i)
      if e and e.name == name then
        return i
      end
    end
  end
  local function failed()
    return said("permission denied") or said("EACCES")
  end
  -- Delete (Replica was never listed here: its folder is empty, the easiest to remove).
  fresh()
  vim.api.nvim_win_set_cursor(0, { line_of("Replica"), 0 })
  keys("dd")
  oil_write(true)
  wait("oil's error", failed)
  unchanged("delete")
  assert(said("only renaming a connection folder"), vim.inspect(messages))
  -- Create a folder and a file.
  fresh()
  vim.api.nvim_buf_set_lines(0, -1, -1, false, { "newdir/", "note.txt" })
  oil_write()
  wait("oil's error", failed)
  unchanged("create")
  -- Copy a connection folder (yank, paste, name the copy).
  fresh()
  vim.api.nvim_win_set_cursor(0, { line_of("Shop"), 0 })
  keys("yyp")
  local l = vim.api.nvim_get_current_line()
  vim.api.nvim_set_current_line((l:gsub("Shop/", "Shop copy/")))
  oil_write()
  wait("oil's error", failed)
  unchanged("copy")
  -- Move a connection folder into another one: cut at the root, paste in Shop (its buffer is
  -- read-only in oil; forced here).
  fresh()
  vim.api.nvim_win_set_cursor(0, { line_of("Staging"), 0 })
  keys("dd")
  vim.cmd("edit " .. vim.fn.fnameescape(mirror .. "/Shop/"))
  wait("oil Shop", function() return oil_ready() and vim.tbl_contains(entries(), "shop.db") and idle() end)
  vim.bo.modifiable = true
  keys("p")
  oil_write() -- one move: a "simple edit", no confirmation
  wait("oil's error", failed)
  vim.cmd("edit!")
  unchanged("move into a folder")
  eq({ "shop.db" }, ls("Shop"), "nothing moved into Shop")
  -- A rename together with anything else: nothing is let through.
  fresh()
  rename_line("Staging", "Stage")
  vim.api.nvim_buf_set_lines(0, -1, -1, false, { "junk/" })
  oil_write()
  wait("oil's error", failed)
  unchanged("rename + create")
  assert(said("only renaming a connection folder"), vim.inspect(messages))
end)

test("oil: a store that can't be written puts the folder back", function()
  oil_root()
  local before, store_before = ls(""), saved()
  -- Another session holds the store's write lock (the store waits 5 s, then gives up).
  local lock = "sqlite:" .. os.getenv("DBEAR_CONNECTIONS_FILE")
  local err = dbear.rpc():request_sync("query.run", { connection = lock, sql = "begin immediate; update connections set position = position" }, 10000)
  assert(not err, vim.inspect(err))
  rename_line("Staging", "Locked")
  oil_write()
  wait("the failure", function() return said("can't rename “Staging” to “Locked”") end, 20000)
  wait("put back", function() return idle() and vim.deep_equal(before, ls("")) end, 10000)
  assert(said("locked"), vim.inspect(messages))
  run(lock, "rollback")
  eq(store_before, saved(), "store unchanged")
  eq("staging-pg", manifest.lookup("Staging").conn.id, "manifest consistent")
  eq("555", mode("Staging"), "read-only")
  wait("oil shows it", function() return oil_ready() and vim.tbl_contains(entries(), "Staging") end)
end)

test("oil: renaming with a table buffer open (pending edit) keeps the buffer working", function()
  oil_root()
  local folder = manifest.connection("shop").dir
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/" .. folder .. "/shop.db/main/users.table"))
  wait("rows", function() return state() and state().ready end)
  local rows = vim.api.nvim_get_current_buf()
  local st = state()
  local l = lines()[2]
  vim.api.nvim_win_set_cursor(0, { 2, l:find("Bob", 1, true) - 1 })
  keys("ciwBobby<Esc>")
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  eq(true, vim.bo.modified, "pending edit")
  eq("644", mode(folder .. "/shop.db/main/users.table"), "writable while loaded")
  vim.cmd("split")
  oil_root()
  rename_line(folder, "Renamed")
  oil_write()
  wait("the store", function() return vim.tbl_contains(saved(), "shop=Renamed") end)
  wait("listed", function() return idle() and vim.tbl_contains(ls(""), "Renamed") end)
  eq(mirror .. "/Renamed/shop.db/main/users.table", fs.inside(vim.api.nvim_buf_get_name(rows)), "the buffer moved with its file")
  eq(true, vim.bo[rows].modified, "the pending edit is still there")
  eq("644", mode("Renamed/shop.db/main/users.table"), "still writable while loaded")
  eq(-1, vim.fn.bufnr("^" .. mirror .. "/" .. folder .. "/shop.db/main/users.table$"), "no buffer left on the old path")
  vim.cmd("close")
  vim.api.nvim_set_current_buf(rows)
  vim.cmd("write")
  wait("confirmation", function() return st.confirm_win ~= nil end)
  keys("y")
  wait("saved", function() return not st.saving and not st.loading and not vim.bo[rows].modified end)
  eq({ { "Bobby" } }, run("shop", "select name from users where id = 2"), "saved in the database")
  keys("-")
  wait("oil folder", function() return oil_ready() and vim.tbl_contains(entries(), "users.table") end)
  eq("oil://" .. mirror .. "/Renamed/shop.db/main/", bufname(), "- goes to the new folder")
  vim.cmd("bwipeout! " .. rows)
  eq("444", mode("Renamed/shop.db/main/users.table"), "read-only again once closed")
end)

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
