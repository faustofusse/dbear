-- The dbear:// browser, driven with real Vim commands. Run by tests/run.sh against a temp store
-- holding "Shop" (SQLite; see run.sh for the data). Every save is checked in the database.
vim.cmd("runtime plugin/dbear.lua")
local dbear = require("dbear")
dbear.setup({ bin = os.getenv("DBEAR_TEST_BIN"), browse = { page_size = 100, max_width = 40 } })
local grid = require("dbear.browse.grid")
local ui = require("dbear.browse.ui")

local failures, passed = 0, 0
local function print(s)
  io.stdout:write(s, "\n")
end

local messages = {}
vim.notify = function(msg, level)
  messages[#messages + 1] = { msg = msg, level = level }
end
local function last_message()
  return (messages[#messages] or {}).msg or ""
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
  -- Leave nothing behind: close floats, back to a plain buffer.
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    if vim.api.nvim_win_get_config(w).relative ~= "" then
      pcall(vim.api.nvim_win_close, w, true)
    end
  end
  vim.cmd("silent! only")
  vim.cmd("enew!")
end

local function eq(want, got, what)
  if not vim.deep_equal(want, got) then
    error(string.format("%s:\nwant %s\n got %s", what or "value", vim.inspect(want), vim.inspect(got)), 2)
  end
end

local function wait(what, check, ms)
  if not vim.wait(ms or 10000, check, 10) then
    error("timed out waiting for " .. what, 2)
  end
end

local function lines()
  return vim.api.nvim_buf_get_lines(0, 0, -1, false)
end

--- The lines without their (concealed) row ids.
local function shown()
  return vim.tbl_map(function(l) return (l:gsub("^/%d+:", "")) end, lines())
end

local function state()
  return grid.states[vim.api.nvim_get_current_buf()]
end

local function idle(st)
  return st and st.ready and not st.loading and not st.saving
end

--- Opens a dbear:// name and waits for it.
local function open(name)
  vim.cmd.edit(vim.fn.fnameescape(name))
  if name:match("^dbear://[^/]*/[^/]*%.") or name:match("^dbear://[^/]*/[^/]*/[^/]+$") then
    wait("table " .. name, function() return idle(state()) end)
    if vim.bo.modified then -- left over from a failed test
      vim.cmd("edit!")
      wait("reload " .. name, function() return idle(state()) end)
    end
  else
    wait("list " .. name, function() return (lines()[1] or "") ~= "Loading…" end)
  end
end

--- Types keys like a user (mappings apply), then lets the buffer react as it does in a UI.
local function keys(k)
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(k, true, false, true), "mx", false)
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = vim.api.nvim_get_current_buf(), modeline = false })
end

--- Puts the cursor on `text` in line `lnum`.
local function cursor_on(lnum, text)
  local col = lines()[lnum]:find(text, 1, true)
  assert(col, "no " .. text .. " in line " .. lnum .. ": " .. lines()[lnum])
  vim.api.nvim_win_set_cursor(0, { lnum, col - 1 })
end

--- A query on Shop (through the CLI's server, like the plugin).
local function db(sql)
  local err, res = dbear.rpc():request_sync("query.run", { connection = "Shop", sql = sql }, 10000)
  assert(not err, vim.inspect(err))
  assert(res.result.ok, vim.inspect(res.result))
  return res.result.ok.rows
end

--- `:w`, then answers the confirmation with `answer`; returns the SQL that was shown.
local function save(answer)
  local st = state()
  vim.cmd("write")
  wait("the confirmation", function() return st.confirm_win ~= nil or not st.saving end)
  if not st.confirm_win then
    vim.wait(50) -- deliver the scheduled message
    return nil
  end
  local buf = vim.api.nvim_win_get_buf(st.confirm_win)
  local sql = vim.api.nvim_buf_get_lines(buf, 2, -1, false)
  keys(answer or "y")
  wait("the save", function() return idle(st) end)
  return sql
end

--- Puts the cursor on the first character of cell `k` of line `lnum`.
local function cell(lnum, k)
  local cells = require("dbear.browse.cells")
  local line = lines()[lnum]
  local p = assert(cells.parse(line, #state().columns), "line " .. lnum .. " can't be read")
  vim.api.nvim_win_set_cursor(0, { lnum, cells.position(line, p, k, 0) })
end

--- The line with a cell that reads exactly `text`.
local function find_line(text)
  local cells = require("dbear.browse.cells")
  for i, l in ipairs(lines()) do
    local p = cells.parse(l, #state().columns)
    if p and vim.tbl_contains(p.cells, text) then
      return i
    end
  end
  error("no line with " .. text)
end

-- MARK: Navigation

test("the tree: connections, tables, a table and back with -", function()
  open("dbear://")
  eq({ "Shop  SQLite · " .. os.getenv("HOME") .. "/shop.db" }, lines(), "connections")
  eq(false, vim.bo.modifiable, "read-only list")
  keys("<CR>")
  wait("tables", function() return lines()[2] == "main" end)
  eq("dbear://Shop/", vim.api.nvim_buf_get_name(0), "name")
  local l = lines()
  assert(l[1]:match("shop%.db · tables$"), l[1])
  assert(vim.tbl_contains(l, "  users   table  ~7"), vim.inspect(l))
  assert(vim.tbl_contains(l, "  named   view   "), vim.inspect(l))
  vim.api.nvim_win_set_cursor(0, { vim.fn.match(l, "^  users ") + 1, 0 })
  keys("<CR>")
  wait("users", function() return idle(state()) end)
  eq("dbear://Shop/main.users", vim.api.nvim_buf_get_name(0), "table name")
  keys("-")
  wait("back up", function() return vim.api.nvim_buf_get_name(0) == "dbear://Shop/" end)
  keys("-")
  eq("dbear://", vim.api.nvim_buf_get_name(0), "root")
end)

test("a table renders NULL, '', 'NULL', escapes, cut and binary cells distinctly", function()
  open("dbear://Shop/main.users")
  eq({
    " 1 | Ana    | ana@x.io | \\N                                       |   1.5 | 0x0102",
    " 2 | 日本語 | jp@x.io  |                                          |    20 | \\N",
    " 3 | Bo     | bo@x.io  | NULL                                     |    \\N | \\N",
    " 4 | Pipe   | p@x.io   | a\\|b\\\\c                                  |     2 | \\N",
    " 5 | Multi  | m@x.io   | two\\nlines                               |     3 | \\N",
    " 6 | Long   | l@x.io   | Lorem ipsum dolor sit amet, consectetur… |     4 | \\N",
    " 9 | Gone   | g@x.io   | \\N                                       |    \\N | \\N",
  }, shown(), "rows")
  eq("id │ name   │ email    │ note                                     │ score │ data  ", state().header, "header")
  eq("%!v:lua.require'dbear.browse.grid'.winbar()", vim.wo.winbar, "winbar")
  eq(2, vim.wo.conceallevel, "ids concealed")
  eq(false, vim.bo.modified, "not modified")
  local before = lines()
  keys("u") -- loading isn't an undo step
  eq(before, lines(), "undo after load")
end)

-- MARK: Editing

test("cw on a cell is an UPDATE of that cell only", function()
  open("dbear://Shop/main.users")
  cursor_on(1, "Ana")
  keys("cwAnna<Esc>")
  eq(true, vim.bo.modified, "modified")
  eq(" 1 | Anna   | ana@x.io | \\N                                       |   1.5 | 0x0102", shown()[1], "realigned")
  local sql = save("y")
  eq({ [[UPDATE "main"."users" SET "name" = 'Anna' WHERE "id" = 1;]] }, sql, "preview")
  eq({ { "Anna", vim.NIL, { float = 1.5 } } }, db("select name, note, score from users where id = 1"), "saved")
  eq(false, vim.bo.modified, "clean after save")
end)

test("dd is a DELETE", function()
  open("dbear://Shop/main.users")
  vim.api.nvim_win_set_cursor(0, { find_line("g@x.io"), 5 })
  keys("dd")
  eq(6, #lines(), "line gone")
  eq({ [[DELETE FROM "main"."users" WHERE "id" = 9;]] }, save("y"), "preview")
  eq({ { 0 } }, db("select count(*) from users where id = 9"), "deleted")
end)

test("yyp then edits is an INSERT copying the untouched cells", function()
  open("dbear://Shop/main.users")
  vim.api.nvim_win_set_cursor(0, { 2, 5 })
  keys("yyp")
  cell(3, 1)
  keys("ciw7<Esc>")
  cursor_on(3, "jp@x.io")
  keys("cWjp2@x.io<Esc>")
  local sql = save("y")
  eq({ [[INSERT INTO "main"."users" ("id", "name", "email", "note", "score", "data") VALUES ('7', '日本語', 'jp2@x.io', '', '20', NULL);]] }, sql, "preview")
  eq({ { 7, "日本語", "jp2@x.io", "", { float = 20 }, vim.NIL } }, db("select * from users where id = 7"), "copied")
end)

test("o adds a row whose empty cells are DEFAULT", function()
  open("dbear://Shop/main.orders")
  vim.api.nvim_win_set_cursor(0, { 3, 5 })
  keys("o2<Esc>")
  eq({ [[INSERT INTO "main"."orders" ("user_id") VALUES ('2');]] }, save("y"), "preview")
  eq({ { 13, 2, 0, "new" } }, db("select * from orders where id = 13"), "defaults")
end)

test(":s and visual-block edits over several rows", function()
  open("dbear://Shop/main.orders")
  vim.cmd("%s/paid/done/")
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  cursor_on(1, "5")
  keys("<C-v>3j<Esc>") -- just a selection: nothing changes
  cell(1, 2)
  keys("<C-v>2jc4<Esc>") -- user_id of the first three rows → 4
  local sql = save("y")
  eq({
    [[UPDATE "main"."orders" SET "user_id" = '4', "status" = 'done' WHERE "id" = 10;]],
    [[UPDATE "main"."orders" SET "user_id" = '4' WHERE "id" = 11;]],
    [[UPDATE "main"."orders" SET "user_id" = '4', "status" = 'done' WHERE "id" = 12;]],
  }, sql, "preview")
  eq({ { 10, 4, "done" }, { 11, 4, "new" }, { 12, 4, "done" }, { 13, 2, "new" } }, db("select id, user_id, status from orders order by id"), "saved")
end)

test("undo and redo, then save: only what's in the buffer counts", function()
  open("dbear://Shop/main.users")
  local before = lines()
  cell(find_line("p@x.io"), 2)
  keys("cwPiper<Esc>")
  eq(true, vim.bo.modified, "modified")
  keys("u")
  eq(before, lines(), "undone (and the realign with it)")
  eq(false, vim.bo.modified, "nothing pending after undo")
  keys("<C-r>")
  eq(true, vim.bo.modified, "redone")
  eq({ [[UPDATE "main"."users" SET "name" = 'Piper' WHERE "id" = 4;]] }, save("y"), "preview")
  eq({ { "Piper", "a|b\\c" } }, db("select name, note from users where id = 4"), "saved; the escaped note untouched")
end)

test("moving lines changes nothing", function()
  open("dbear://Shop/main.users")
  vim.api.nvim_win_set_cursor(0, { 1, 6 })
  keys("ddp")
  vim.api.nvim_win_set_cursor(0, { 5, 6 })
  keys("ddggP")
  vim.cmd("g/^/m0") -- reverse every line
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  eq(false, vim.bo.modified, "not modified")
  eq(nil, save("y"), "no confirmation")
  eq("dbear: no changes", last_message(), "message")
end)

test("NULL, empty string and the text NULL are typed distinctly", function()
  open("dbear://Shop/main.users")
  local l3, l4 = find_line("bo@x.io"), find_line("p@x.io")
  cell(l3, 4) -- 'NULL' (text) → NULL
  keys("C\\N<Esc>")
  -- `C` took the rest of the line: put the last cells back.
  keys("A | \\N | \\N<Esc>")
  cell(l4, 4) -- 'a|b\c' → '' (an empty cell; `df|` would stop at the escaped pipe)
  keys("dW")
  local l1 = find_line("ana@x.io")
  cell(l1, 4) -- NULL → the text NULL
  keys("2xiNULL<Esc>")
  local sql = save("y")
  eq({
    [[UPDATE "main"."users" SET "note" = 'NULL' WHERE "id" = 1;]],
    [[UPDATE "main"."users" SET "note" = NULL WHERE "id" = 3;]],
    [[UPDATE "main"."users" SET "note" = '' WHERE "id" = 4;]],
  }, sql, "preview")
  eq({ { 1, "NULL" }, { 3, vim.NIL }, { 4, "" } }, db("select id, note from users where id in (1, 3, 4) order by id"), "saved")
end)

test("a value holding the separator, escapes and unicode round-trip", function()
  open("dbear://Shop/main.users")
  local l = find_line("p@x.io")
  cell(l, 4) -- empty: the cursor is on the next separator
  keys("ix\\|y\\\\z\\tw<Esc>")
  local l2 = find_line("jp@x.io")
  cell(l2, 2)
  keys("ciw日本<Esc>")
  local sql = save("y")
  eq({
    [[UPDATE "main"."users" SET "name" = '日本' WHERE "id" = 2;]],
    "UPDATE \"main\".\"users\" SET \"note\" = 'x|y\\z\tw' WHERE \"id\" = 4;",
  }, sql, "preview")
  eq({ { "日本", "" }, { "Piper", "x|y\\z\tw" } }, db("select name, note from users where id in (2, 4) order by id"), "saved")
  assert(lines()[find_line("p@x.io")]:find("x\\|y\\\\z\\tw", 1, true), "shown escaped again")
end)

test("cut, multi-line and binary cells are only written through <CR>", function()
  open("dbear://Shop/main.users")
  local long = find_line("l@x.io")
  -- Editing another cell of the row leaves the cut note alone.
  cell(long, 5)
  keys("ciw40<Esc>")
  local multi = find_line("m@x.io")
  cell(multi, 2)
  keys("ciwMulti2<Esc>")
  eq({
    [[UPDATE "main"."users" SET "name" = 'Multi2' WHERE "id" = 5;]],
    [[UPDATE "main"."users" SET "score" = '40' WHERE "id" = 6;]],
  }, save("y"), "only the edited cells")
  local full = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore"
  eq({ { "two\nlines", full } }, db("select (select note from users where id = 5), (select note from users where id = 6)"), "untouched")

  -- Changing the cut text in the line is refused at :w, with the line number.
  long = find_line("l@x.io")
  vim.cmd(long .. "s/Lorem/Ipsum/")
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  eq(nil, save("y"), "no confirmation")
  assert(last_message():find("line " .. long .. ": note: the value is cut", 1, true), last_message())
  local diags = vim.diagnostic.get(0)
  eq(long - 1, diags[1] and diags[1].lnum, "diagnostic on the line")
  keys("u")
  -- So is text in a binary cell; \N is fine.
  local ana = find_line("ana@x.io")
  cell(ana, 6)
  keys("cW0x0103<Esc>")
  eq(nil, save("y"), "binary refused")
  assert(last_message():find("data: binary values can't be edited", 1, true), last_message())
  cell(ana, 6)
  keys("cW\\N<Esc>")
  eq({ [[UPDATE "main"."users" SET "data" = NULL WHERE "id" = 1;]] }, save("y"), "binary to NULL")

  -- <CR> opens the whole value; :w there puts it in the row; :w in the table saves it.
  long = find_line("l@x.io")
  cell(long, 4)
  keys("<CR>")
  eq({ full }, lines(), "the whole value")
  vim.cmd("s/labore/labore et dolore/")
  vim.cmd("write")
  vim.cmd("close")
  assert(lines()[long]:find("labore et dolore", 1, true), "the whole value is in the line")
  multi = find_line("m@x.io")
  cell(multi, 4)
  keys("<CR>")
  eq({ "two", "lines" }, lines(), "multi-line value")
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "three", "lines", "now" })
  vim.cmd("write")
  vim.cmd("close")
  eq({
    [[UPDATE "main"."users" SET "note" = 'three]],
    "lines",
    [[now' WHERE "id" = 5;]],
    [[UPDATE "main"."users" SET "note" = ']] .. full .. [[ et dolore' WHERE "id" = 6;]],
  }, save("y"), "staged values")
  eq({ { "three\nlines\nnow" }, { full .. " et dolore" } }, db("select note from users where id in (5, 6) order by id"), "saved whole")
end)

test("lines that can't be read are reported with their numbers, never guessed", function()
  open("dbear://Shop/main.users")
  local jp, pipe = find_line("jp@x.io"), find_line("p@x.io")
  vim.cmd(jp .. "s/jp@x.io/jp|x/")
  vim.cmd(pipe .. "s/\\\\|/|/") -- an escaped pipe loses its backslash: one cell too many
  vim.api.nvim_buf_set_lines(0, 5, 5, false, { "just text" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  eq(nil, save("y"), "nothing to confirm")
  local msg = last_message()
  assert(msg:find("line " .. jp .. ": 7 cells, expected 6", 1, true), msg)
  assert(msg:find("line 6: 1 cells, expected 6", 1, true), msg)
  eq(0, state().diff.counts.delete, "rows on unreadable lines aren't deletes")
  eq(true, vim.bo.modified, "still pending")
  eq({ { "jp@x.io" } }, db("select email from users where id = 2"), "untouched")
end)

test("cancelling the confirmation writes nothing and keeps the edits", function()
  open("dbear://Shop/main.users")
  vim.cmd("edit!") -- drop what the last test left
  wait("reload", function() return idle(state()) end)
  cell(1, 2)
  keys("ciwNope<Esc>")
  local sql = save("n")
  eq({ [[UPDATE "main"."users" SET "name" = 'Nope' WHERE "id" = 1;]] }, sql, "shown")
  eq("dbear: not saved", last_message(), "message")
  eq(true, vim.bo.modified, "still modified")
  assert(lines()[1]:find("Nope", 1, true), "edit kept")
  eq({ { "Anna" } }, db("select name from users where id = 1"), "untouched")
  save("<Esc>")
  eq({ { "Anna" } }, db("select name from users where id = 1"), "untouched (Esc)")
end)

test("a failing save rolls everything back and keeps the edits", function()
  open("dbear://Shop/main.users")
  vim.cmd("edit!")
  wait("reload", function() return idle(state()) end)
  cell(1, 2)
  keys("ciwAnnie<Esc>")
  local l2 = find_line("jp@x.io")
  cell(l2, 3)
  keys("cWana@x.io<Esc>") -- unique email: fails
  local sql = save("y")
  eq(2, #sql, "two updates shown")
  assert(state().error and state().error:find("UNIQUE"), tostring(state().error))
  eq({ { "Anna", "jp@x.io" } }, db("select (select name from users where id = 1), (select email from users where id = 2)"), "rolled back")
  eq(true, vim.bo.modified, "edits kept")
  assert(lines()[1]:find("Annie", 1, true), "edit still there")
  cell(l2, 3)
  keys("cWjp3@x.io<Esc>")
  save("y")
  eq(nil, state().error, "error cleared")
  eq({ { "Annie", "jp3@x.io" } }, db("select (select name from users where id = 1), (select email from users where id = 2)"), "saved")
end)

test("paging: more rows on demand, with edits pending on two pages", function()
  open("dbear://Shop/main.big")
  eq(100, #lines(), "first page")
  cell(1, 2)
  keys("ciwfirst<Esc>")
  vim.api.nvim_win_set_cursor(0, { 100, 6 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
  vim.wait(300)
  eq(100, #lines(), "no automatic load while edits are pending")
  keys("]]")
  wait("page 2", function() return #lines() == 200 and idle(state()) end)
  assert(lines()[1]:find("first", 1, true), "edit on page 1 kept")
  cell(150, 2)
  keys("ciwmiddle<Esc>")
  keys("]]")
  wait("page 3", function() return #lines() == 250 and idle(state()) end)
  eq(nil, state().next, "all loaded")
  eq({
    [[UPDATE "main"."big" SET "v" = 'first' WHERE "id" = 1;]],
    [[UPDATE "main"."big" SET "v" = 'middle' WHERE "id" = 150;]],
  }, save("y"), "both pages")
  eq({ { "first" }, { "middle" } }, db("select v from big where id in (1, 150) order by id"), "saved")
  -- Nothing pending: the next page loads near the end by itself.
  eq(100, #lines(), "reloaded to one page")
  vim.api.nvim_win_set_cursor(0, { 99, 6 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
  wait("automatic page", function() return #lines() == 200 and idle(state()) end)
end)

test("sort and filter reload, and ask before dropping edits", function()
  open("dbear://Shop/main.big")
  vim.cmd("DbearSort id desc")
  wait("sorted", function() return idle(state()) and shown()[1]:match("^250 ") end)
  cell(1, 1)
  keys("gs") -- ascending by id
  wait("asc", function() return idle(state()) and shown()[1]:match("^  1 ") end)
  vim.cmd("DbearFilter id % 50 = 0")
  wait("filtered", function() return idle(state()) and #lines() == 5 end)
  eq("250 | v250", shown()[5], "filtered rows")
  cell(1, 2)
  keys("ciwfifty<Esc>")
  local asked
  ui.ask = function(q)
    asked = q
    return false
  end
  vim.cmd("DbearFilter")
  assert(asked and asked:find("Discard"), "asked")
  eq(5, #lines(), "kept the filter")
  assert(lines()[1]:find("fifty", 1, true), "kept the edit")
  ui.ask = function() return true end
  vim.cmd("DbearFilter")
  wait("unfiltered", function() return idle(state()) and #lines() == 100 end)
  eq(false, vim.bo.modified, "edits dropped")
  vim.cmd("DbearFilter nope = ")
  wait("error", function() return idle(state()) and last_message():find("dbear") end)
  eq(100, #lines(), "a bad filter keeps the rows")
end)

test("foreign keys: gf to the row it points at, gR to the rows referencing it", function()
  open("dbear://Shop/main.orders")
  cell(1, 2) -- order 10's user_id: 4
  keys("gf")
  wait("users row", function() return vim.api.nvim_buf_get_name(0):find("main.users?where", 1, true) and idle(state()) end)
  eq(1, #lines(), "one row")
  assert(shown()[1]:find("bo@x.io", 1, true) == nil, "not Bo")
  eq('"id" = 4', state().query.filter, "filter")
  open("dbear://Shop/main.users")
  cell(find_line("p@x.io"), 1)
  keys("gR")
  wait("orders of user 4", function() return vim.api.nvim_buf_get_name(0):find("main.orders?where", 1, true) and idle(state()) end)
  eq('"user_id" = 4', state().query.filter, "filter")
  eq(3, #lines(), "user 4's orders")
  keys("-")
  wait("tables", function() return vim.api.nvim_buf_get_name(0) == "dbear://Shop/" end)
end)

test("views and tables without a primary key are read-only, with the reason", function()
  open("dbear://Shop/main.named")
  eq(false, vim.bo.modifiable, "view")
  assert(state().read_only:find("view"), state().read_only)
  open("dbear://Shop/main.logs")
  eq(false, vim.bo.modifiable, "no key")
  assert(state().read_only:find("no primary key"), state().read_only)
  local footer = vim.api.nvim_buf_get_extmarks(0, -1, 0, -1, { details = true, type = "virt_lines" })
  local text = vim.inspect(footer)
  assert(text:find("read-only", 1, true), text)
end)

test("a composite primary key names rows by all its columns", function()
  open("dbear://Shop/main.pairs")
  local l = find_line("two")
  cell(l, 3)
  keys("ciwdeux<Esc>")
  cell(find_line("three"), 2)
  keys("ciw5<Esc>")
  vim.api.nvim_win_set_cursor(0, { 1, 6 })
  keys("dd")
  eq({
    [[DELETE FROM "main"."pairs" WHERE "a" = 'x' and "b" = 1;]],
    [[UPDATE "main"."pairs" SET "v" = 'deux' WHERE "a" = 'x' and "b" = 2;]],
    [[UPDATE "main"."pairs" SET "b" = '5' WHERE "a" = 'y' and "b" = 1;]],
  }, save("y"), "preview")
  eq({ { "x", 2, "deux" }, { "y", 5, "three" } }, db("select * from pairs order by a"), "saved")
end)

test(":e! discards pending edits", function()
  open("dbear://Shop/main.pairs")
  cell(1, 3)
  keys("ciwgone<Esc>")
  eq(true, vim.bo.modified, "modified")
  vim.cmd("edit!")
  wait("reload", function() return idle(state()) end)
  eq(false, vim.bo.modified, "clean")
  assert(not lines()[1]:find("gone", 1, true), "discarded")
end)

test("copies take the whole value of untouched cut cells; binary values aren't copied", function()
  open("dbear://Shop/main.users")
  local full = db("select note from users where id = 6")[1][1]
  vim.api.nvim_win_set_cursor(0, { find_line("l@x.io"), 6 })
  keys("yyP")
  local copy = vim.api.nvim_win_get_cursor(0)[1]
  cell(copy, 1)
  keys("ciw\\D<Esc>") -- id: DEFAULT
  cell(copy, 3)
  keys("cWl2@x.io<Esc>")
  local sql = save("y")
  eq({ [[INSERT INTO "main"."users" ("name", "email", "note", "score", "data") VALUES ('Long', 'l2@x.io', ']] .. full .. [[', '40', NULL);]] }, sql, "preview")
  eq({ { full } }, db("select note from users where email = 'l2@x.io'"), "whole value")

  open("dbear://Shop/main.files")
  keys("yyp")
  cell(2, 1)
  keys("ciw2<Esc>")
  eq(nil, save("y"), "refused")
  assert(last_message():find("line 2: data: a binary value can't be copied", 1, true), last_message())
  cell(2, 2)
  keys("cW\\D<Esc>")
  eq({ [[INSERT INTO "main"."files" ("id") VALUES ('2');]] }, save("y"), "without the blob")
end)

test("\\e is the empty string in a new row, an empty cell DEFAULT", function()
  open("dbear://Shop/main.orders")
  keys("G")
  keys("o2<Esc>") -- the new row's first non-key cell
  local l = vim.api.nvim_win_get_cursor(0)[1]
  cell(l, 3) -- empty: on the separator after it
  keys("i1<Esc>")
  keys("A\\e<Esc>")
  eq({ [[INSERT INTO "main"."orders" ("user_id", "total", "status") VALUES ('2', '1', '');]] }, save("y"), "preview")
  eq({ { 2, 1, "" } }, db("select user_id, total, status from orders where id = 14"), "saved")
end)

test(":DbearOpen, g? and the cursor kept off the row ids", function()
  vim.cmd("DbearOpen Shop/main.pairs")
  wait("pairs", function() return idle(state()) end)
  eq("dbear://Shop/main.pairs", vim.api.nvim_buf_get_name(0), "name")
  vim.api.nvim_win_set_cursor(0, { 1, 0 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
  eq(5, vim.api.nvim_win_get_cursor(0)[2], "cursor on the first cell's text")
  keys("g?")
  local buf = vim.api.nvim_get_current_buf()
  assert(vim.api.nvim_buf_get_lines(buf, 0, 1, false)[1] == "Keys", "help float")
  keys("q")
  keys("gd")
  wait("structure", function()
    local b = vim.fn.bufnr("dbear://results")
    return b ~= -1 and (vim.api.nvim_buf_get_lines(b, 0, 1, false)[1] or ""):find("main.pairs", 1, true)
  end)
  vim.cmd("silent! only")
  cell(1, 3)
  keys("ciwchanged<Esc>")
  local asked
  ui.ask = function(q)
    asked = q
    return false
  end
  keys("<C-l>")
  assert(asked, "<C-l> asks first")
  eq(true, vim.bo.modified, "kept")
  ui.ask = function() return true end
  keys("<C-l>")
  wait("refreshed", function() return idle(state()) and not vim.bo.modified end)
  vim.cmd("DbearOpen!") -- the dbear:// list (without !, the mirror's root)
  wait("root", function() return vim.api.nvim_buf_get_name(0) == "dbear://" and lines()[1] ~= "Loading…" end)
end)

test("a line pasted from another table buffer is a new row there", function()
  open("dbear://Shop/main.pairs")
  vim.api.nvim_win_set_cursor(0, { 1, 6 })
  keys("yy")
  open("dbear://Shop/main.pairs?where=a%20%3D%20'y'")
  eq(1, #lines(), "filtered: another buffer of the same table")
  keys("p")
  cell(2, 2)
  keys("ciw7<Esc>")
  eq({ [[INSERT INTO "main"."pairs" ("a", "b", "v") VALUES ('x', '7', 'deux');]] }, save("y"), "an insert, not an update")
  eq({ { 1 } }, db("select count(*) from pairs where a = 'x' and b = 2"), "the original is untouched")
end)

test("the confirmation says when a primary key changes", function()
  open("dbear://Shop/main.pairs")
  cell(1, 2)
  keys("ciw8<Esc>")
  local st = state()
  vim.cmd("write")
  wait("confirmation", function() return st.confirm_win ~= nil end)
  local first = vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(st.confirm_win), 0, 1, false)[1]
  eq("1 update in main.pairs · changes the primary key of 1 row", first, "summary")
  keys("q")
  wait("cancel", function() return not st.saving end)
end)

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
