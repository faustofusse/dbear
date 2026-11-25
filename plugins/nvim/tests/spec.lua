-- Run by tests/run.sh: nvim --headless -u NONE -l spec.lua, with a seeded temp store ("App", SQLite).
vim.cmd("runtime plugin/dbear.lua")
local dbear = require("dbear")
dbear.setup({ bin = os.getenv("DBEAR_TEST_BIN"), max_width = 20 })

local failures, passed = 0, 0
local function print(s)
  io.stdout:write(s, "\n")
end
local function test(name, fn)
  local ok, err = pcall(fn)
  if ok then
    passed = passed + 1
    print("ok   " .. name)
  else
    failures = failures + 1
    print("FAIL " .. name .. "\n     " .. tostring(err))
  end
end

local function eq(want, got, what)
  if not vim.deep_equal(want, got) then
    error(string.format("%s:\nwant %s\n got %s", what or "value", vim.inspect(want), vim.inspect(got)), 2)
  end
end

local function results()
  local buf = vim.fn.bufnr("dbear://results")
  return buf ~= -1 and vim.api.nvim_buf_get_lines(buf, 0, -1, false) or {}
end

--- Waits until the results buffer's first line matches `pattern`.
local function wait_results(pattern)
  local ok = vim.wait(10000, function() return (results()[1] or ""):match(pattern) ~= nil end, 20)
  if not ok then
    error("results never matched " .. pattern .. ":\n" .. table.concat(results(), "\n"), 2)
  end
  return results()
end

local function sql_buffer(lines)
  vim.cmd("enew")
  vim.bo.filetype = "sql"
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
end

test("connections are listed and :Dbear picks one", function()
  eq({ "App" }, dbear.connection_names(), "names")
  sql_buffer({ "select 1" })
  local offered
  vim.ui.select = function(items, opts, on_choice)
    offered = vim.tbl_map(opts.format_item, items)
    on_choice(items[1])
  end
  vim.cmd("Dbear")
  vim.wait(5000, function() return vim.b.dbear_connection ~= nil end, 20)
  eq({ "App  SQLite · " .. os.getenv("HOME") .. "/app.db" }, offered, "select items")
  eq("app", vim.b.dbear_connection, "b:dbear_connection")
  eq("v:lua.require'dbear'.omnifunc", vim.bo.omnifunc, "omnifunc")
end)

test(":Dbear name sets the connection directly", function()
  sql_buffer({ "" })
  vim.cmd("Dbear app") -- the id; names work too (case-insensitive)
  vim.wait(5000, function() return vim.b.dbear_connection ~= nil end, 20)
  eq("App", vim.b.dbear_connection_name, "name")
  local url = "sqlite:" .. os.getenv("HOME") .. "/app.db"
  vim.cmd("Dbear " .. url) -- a URL is kept as is: its "url:…" id dies with the server
  vim.wait(5000, function() return vim.b.dbear_connection == url end, 20)
  eq(url, vim.b.dbear_connection, "URL spec")
end)

test(":DbearRun renders an aligned table", function()
  vim.cmd("Dbear App")
  vim.wait(5000, function() return vim.b.dbear_connection == "app" end, 20)
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select id, name, score, note,", "  9007199254740993 as big", "from users order by id" })
  vim.cmd("DbearRun")
  local lines = wait_results("^3 rows")
  lines[1] = lines[1]:gsub("%d+ ms", "N ms")
  eq({
    "3 rows · N ms",
    " id │ name   │ score │ note       │              big ",
    "────┼────────┼───────┼────────────┼──────────────────",
    "  1 │ Ana    │   1.5 │ NULL       │ 9007199254740993 ",
    "  2 │ 日本語 │    20 │ NULL       │ 9007199254740993 ",
    "  3 │ Bo     │  NULL │ two\\nlines │ 9007199254740993 ",
  }, lines, "results")
  -- The real NULLs are highlighted, the text 'NULL' (row 2's note) isn't.
  local buf = vim.fn.bufnr("dbear://results")
  local ns = vim.api.nvim_get_namespaces().dbear
  local nulls = {}
  for _, m in ipairs(vim.api.nvim_buf_get_extmarks(buf, ns, 0, -1, { details = true })) do
    if m[4].hl_group == "DbearNull" then
      nulls[#nulls + 1] = { m[2], lines[m[2] + 1]:sub(m[3] + 1, m[4].end_col) }
    end
  end
  eq({ { 3, "NULL" }, { 5, "NULL" } }, nulls, "NULL highlights")
end)

test(":DbearRun on a range runs only those lines", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select 'one' as a;", "select 'two' as b, 'é' as c" })
  vim.cmd("2DbearRun")
  local lines = wait_results("^1 row ")
  eq({ " b   │ c ", "─────┼───", " two │ é " }, { lines[2], lines[3], lines[4] }, "range result")
end)

test(":'<,'>DbearRun runs a charwise visual selection", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select 1 as a;", "select 'two' as b, 'é' as c" })
  vim.cmd("normal! 2G0v16l\27") -- selects "select 'two' as b"
  vim.cmd("'<,'>DbearRun")
  local lines = wait_results("^1 row ")
  eq({ " b   ", "─────", " two " }, { lines[2], lines[3], lines[4] }, "selection result")
end)

test("wide cells are cut to max_width display cells", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select 'ａｂｃｄｅｆｇｈｉｊｋｌ' as wide" })
  vim.cmd("DbearRun")
  local lines = wait_results("^1 row ")
  eq(" ａｂｃｄｅｆｇｈｉ… ", lines[4], "cut cell")
end)

test("a failing statement shows the error in the first line", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select * from ghost" })
  vim.cmd("DbearRun")
  local lines = wait_results("^Error")
  assert(lines[1]:match("^Error: .*no such table: ghost · %d+ ms$"), lines[1])
end)

test("statements without rows report rows affected", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "update users set score = score where id < 3" })
  vim.cmd("DbearRun")
  wait_results("^2 rows affected · %d+ ms$")
end)

test(":DbearCancel stops a long query", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, {
    "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c",
  })
  vim.cmd("DbearRun")
  wait_results("^Running")
  vim.wait(300)
  eq("Running… (:DbearCancel stops it)", results()[1], "still running")
  vim.cmd("DbearCancel")
  wait_results("^Cancelled · %d+ ms$")
end)

test(":DbearTables and :DbearDescribe", function()
  vim.cmd("DbearTables")
  local lines = wait_results("^1 table")
  assert(lines[4]:match("^ main%s+│ users%s+│ table │%s+3 $"), lines[4])
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select * from users" })
  vim.api.nvim_win_set_cursor(0, { 1, 16 })
  vim.cmd("DbearDescribe")
  lines = wait_results("^main%.users")
  assert(lines[4]:match("^ id%s+│ integer │ no%s+│%s+│ PK"), lines[4])
  assert(vim.tbl_contains(lines, "DDL:"), "has DDL")
end)

test("completion items from the server (byte offsets)", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "-- ünïcödé", "select na from us" })
  vim.api.nvim_win_set_cursor(0, { 2, 17 }) -- end of line; line 1 has multibyte characters
  eq(15, dbear.omnifunc(1, ""), "table start")
  local items = dbear.omnifunc(0, "us")
  eq("users", items[1] and items[1].word, "table item")
  eq("t", items[1].kind, "kind")
  vim.api.nvim_buf_set_lines(0, 1, 2, false, { "select na from users" })
  vim.api.nvim_win_set_cursor(0, { 2, 9 })
  eq(7, dbear.omnifunc(1, ""), "column start")
  local words = vim.tbl_map(function(i) return i.word end, dbear.omnifunc(0, "na"))
  assert(vim.tbl_contains(words, "name"), vim.inspect(words))
end)

test("<C-x><C-o> in insert mode completes", function()
  vim.api.nvim_buf_set_lines(0, 0, -1, false, { "select * from us" })
  vim.api.nvim_win_set_cursor(0, { 1, 15 })
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes("A<C-x><C-o><Esc>", true, false, true), "x", false)
  eq({ "select * from users" }, vim.api.nvim_buf_get_lines(0, 0, -1, false), "completed line")
end)

test(":DbearHistory lists the runs", function()
  local picked
  vim.ui.select = function(items, opts, on_choice)
    picked = items
    on_choice(nil)
  end
  vim.cmd("DbearHistory")
  vim.wait(5000, function() return picked ~= nil end, 20)
  assert(picked and #picked >= 4, "entries: " .. vim.inspect(picked and #picked))
  for _, e in ipairs(picked) do
    assert(not e.sql:match("recursive"), "cancelled runs aren't recorded")
  end
end)

test("the server restarts after it exits", function()
  local client = dbear.rpc()
  client.proc:kill(9)
  vim.wait(5000, function() return not client:running() end, 20)
  eq({ "App" }, dbear.connection_names(), "names after restart")
end)

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
