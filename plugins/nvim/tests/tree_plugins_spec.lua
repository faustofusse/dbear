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
  wait("oil gave up", function() return not require("oil.mutator").is_mutating() and #messages > 0 end)
  local function denied()
    for _, m in ipairs(messages) do
      if m.msg:lower():find("permission denied", 1, true) then
        return true
      end
    end
  end
  assert(denied(), "oil reports the error: " .. vim.inspect(messages))
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
  wait("oil gave up", function() return not require("oil.mutator").is_mutating() and #messages > 0 end)
  assert(denied(), "oil reports the error: " .. vim.inspect(messages))
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

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
