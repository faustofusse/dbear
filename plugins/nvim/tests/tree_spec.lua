-- The mirror (lua/dbear/tree), driven with real Vim commands. Run by tests/run.sh against a temp
-- store (see run.sh): "Shop" (id shop; SQLite, seeded), "shop" (id other) and "a/b: c" and
-- ".hidden" (another SQLite file), "Staging" (the mock Postgres: several databases and schemas) and "Replica" (a mock
-- Postgres that can't be reached). The mirror root is a temp folder.
local mirror = assert(os.getenv("DBEAR_TEST_MIRROR"))
local outside = assert(os.getenv("DBEAR_TEST_OUTSIDE")) -- a folder next to the mirror
vim.cmd("runtime plugin/dbear.lua")
local dbear = require("dbear")
local started_at_load = dbear.rpc():running()
dbear.setup({ bin = os.getenv("DBEAR_TEST_BIN"), tree = { root = mirror } })
local tree = require("dbear.tree")
local fs = require("dbear.tree.fs")
local manifest = require("dbear.tree.manifest")
local names = require("dbear.tree.names")
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
    if vim.api.nvim_win_get_config(w).relative ~= "" then
      pcall(vim.api.nvim_win_close, w, true)
    end
  end
  vim.cmd("silent! only")
  vim.cmd("enew!")
  vim.cmd("silent! %bwipeout!")
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

local function keys(k)
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(k, true, false, true), "mx", false)
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = vim.api.nvim_get_current_buf(), modeline = false })
end

-- Every request the plugin sends.
local calls = {}
local client = dbear.rpc()
local request = client.request
client.request = function(self, method, params, cb, fast)
  calls[#calls + 1] = { method = method, connection = params and params.connection, database = params and params.database }
  return request(self, method, params, cb, fast)
end
local function methods()
  return vim.tbl_map(function(c) return c.method .. (c.connection and " " .. c.connection or "") end, calls)
end

local function idle()
  return tree.idle()
end

local function ls(rel)
  local out = {}
  for name in fs.list(mirror .. (rel ~= "" and "/" .. rel or "")) do
    out[#out + 1] = name
  end
  table.sort(out)
  return out
end

local function exists(rel)
  return uv.fs_lstat(mirror .. "/" .. rel) ~= nil
end

local function mode(rel)
  return string.format("%o", bit.band(uv.fs_lstat(mirror .. "/" .. rel).mode, tonumber("777", 8)))
end

local function read(rel)
  return fs.read(mirror .. "/" .. rel)
end

local function run(conn, sql)
  local err, res = dbear.rpc():request_sync("query.run", { connection = conn, sql = sql }, 10000)
  assert(not err, vim.inspect(err))
  assert(res.result.ok, vim.inspect(res.result))
  return res.result.ok.rows
end

local function store(sql)
  run("sqlite:" .. os.getenv("DBEAR_CONNECTIONS_FILE"), sql)
end

--- Enters a mirror folder like a user (`:edit <dir>`) and waits for the listing it starts.
local function enter(rel)
  vim.cmd.edit(vim.fn.fnameescape(mirror .. (rel ~= "" and "/" .. rel or "")))
  wait("listing " .. rel, idle)
end

local function sync(arg, bang)
  local done
  tree.sync(arg or "", bang or false, function(e) done = { e } end)
  wait("sync", function() return done ~= nil end, 20000)
  wait("idle", idle)
  return done[1]
end

--- The current buffer's name, under the mirror root as configured (nvim may resolve symlinks
--- in it, e.g. /tmp → /private/tmp on macOS).
local function bufname(buf)
  local name = vim.api.nvim_buf_get_name(buf or 0)
  return fs.inside(name) or name
end

local function state()
  return grid.states[vim.api.nvim_get_current_buf()]
end

local function rows_buffer(rel)
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/" .. rel))
  wait("rows of " .. rel, function()
    local st = state()
    return st and st.ready and not st.loading
  end)
  return state()
end

local function shown()
  return vim.tbl_map(function(l) return (l:gsub("^/%d+:", "")) end, vim.api.nvim_buf_get_lines(0, 0, -1, false))
end

-- MARK: Tests

test("loading the plugin connects to nothing and touches no disk", function()
  eq(false, started_at_load, "server started at load")
  eq(false, dbear.rpc():running(), "server running after setup")
  eq(nil, uv.fs_lstat(mirror), "the mirror root isn't created")
  -- An unrelated directory and file don't start anything either.
  vim.cmd.edit(vim.fn.fnameescape(outside))
  vim.cmd.edit(vim.fn.fnameescape(os.getenv("DBEAR_CONNECTIONS_FILE")))
  eq({}, calls, "requests")
end)

test(":DbearOpen opens the root and lists the connection folders only (no network)", function()
  vim.cmd("DbearOpen")
  eq(mirror, bufname(), "the root in the current window")
  wait("listing", idle)
  eq({ "connections.list" }, methods(), "only the store is read")
  eq({ "%2Ehidden", ".dbear-mirror.json", "Replica", "Shop", "Staging", "a%2Fb%3A c", "shop%~2" }, ls(""), "folders")
  eq({}, ls("Shop"), "not listed before it's entered")
  eq("555", mode(""), "root read-only")
  eq("555", mode("Shop"), "folder read-only")
  eq("Shop", manifest.lookup("Shop").conn.name, "manifest")
  eq("shop", manifest.lookup("shop%~2").conn.name, "case-colliding names: the second one gets %~2")
  eq("other", manifest.lookup("shop%~2").conn.id, "mapped by id")
  eq("a/b: c", manifest.lookup("a%2Fb%3A c").conn.name, "odd name")
  eq(".hidden", names.unescape("%2Ehidden"), "reversible")
end)

test("entering a connection folder lists it (and nothing else)", function()
  calls = {}
  enter("Shop")
  local m = methods()
  assert(vim.tbl_contains(m, "schemas.list shop"), vim.inspect(m))
  assert(vim.tbl_contains(m, "columns.list shop"), vim.inspect(m))
  for _, x in ipairs(m) do
    assert(not x:find("staging") and not x:find("replica"), "other connections untouched: " .. x)
  end
  eq({ "shop.db" }, ls("Shop"), "the SQLite file's name as its database folder")
  eq({ "main" }, ls("Shop/shop.db"), "schema")
  eq({ "big.table", "named.view", "orders.table", "users.table" }, ls("Shop/shop.db/main"), "tables and views")
  eq("444", mode("Shop/shop.db/main/users.table"), "file read-only")
  eq("555", mode("Shop/shop.db/main"), "folder read-only")
  -- Entering it again in the same session doesn't list again.
  calls = {}
  enter("Shop")
  enter("Shop/shop.db/main")
  eq({}, methods(), "listed once per session")
end)

test("table files hold the target and the column list", function()
  local text = read("Shop/shop.db/main/users.table")
  local lines = vim.split(text, "\n", { plain = true })
  local h = require("dbear.tree.sync").parse_header(lines[1])
  eq({ connection = "shop", database = os.getenv("DBEAR_TEST_SHOP"), browses = false, schema = "main", table = "users", kind = "table" }, h, "header")
  assert(text:find("\nid     integer  PK  NOT NULL\n", 1, true), text)
  assert(text:find("\nemail  text         NULL\n", 1, true), text)
  local view = read("Shop/shop.db/main/named.view")
  assert(view:find("\nid ", 1, true) and view:find("\nname ", 1, true), "a view's columns: " .. view)
  eq(nil, text:find("secret", 1, true), "no secrets")
end)

test("a browsing connection: databases first, then each database when entered", function()
  calls = {}
  enter("Staging")
  eq({ "databases.list staging-pg" }, methods(), "only the databases")
  eq({ "app", "app_test", "postgres" }, ls("Staging"), "database folders")
  eq({}, ls("Staging/app"), "not listed yet")
  calls = {}
  enter("Staging/app_test")
  eq({ "schemas.list staging-pg", "columns.list staging-pg" }, methods(), "the database")
  eq("app_test", calls[1].database, "that database")
  eq({ "analytics", "billing", "public" }, ls("Staging/app_test"), "schemas")
  assert(vim.tbl_contains(ls("Staging/app_test/public"), "users.table"), vim.inspect(ls("Staging/app_test/public")))
  eq({}, ls("Staging/app"), "other databases still not listed")
  local h = require("dbear.tree.sync").parse_header(read("Staging/app_test/public/users.table"):match("^[^\n]*"))
  eq({ "staging-pg", "app_test", true }, { h.connection, h.database, h.browses }, "header")
end)

test("an unreachable server: the error is shown, nothing is created", function()
  calls = {}
  enter("Replica")
  eq({ "databases.list prod-replica" }, methods(), "tried")
  eq({}, ls("Replica"), "nothing")
  local st = vim.b.dbear_tree_status or ""
  assert(st:find("✗", 1, true) and st:find("could not connect", 1, true), "status: " .. st)
  assert(messages[#messages].msg:find("could not connect", 1, true), "notified")
  assert(manifest.connection("prod-replica").error, "recorded")
end)

test("a failed listing keeps the previous tree", function()
  local before = ls("Shop/shop.db/main")
  store("update connections set database = '/nonexistent/dir/shop.db' where id = 'shop'")
  local err = sync("shop")
  assert(err, "fails")
  eq(before, ls("Shop/shop.db/main"), "files kept")
  assert(read("Shop/shop.db/main/users.table"):find("email", 1, true), "content kept")
  enter("Shop/shop.db/main")
  local st = vim.b.dbear_tree_status or ""
  assert(st:find("✗", 1, true), "status shows the error: " .. st)
  store("update connections set database = '" .. os.getenv("DBEAR_TEST_SHOP") .. "' where id = 'shop'")
  eq(nil, sync("shop"), "works again")
  enter("Shop/shop.db/main")
  eq(nil, vim.b.dbear_tree_status, "error cleared")
end)

test("sync adds and removes tables, renames and removes connections, on read-only folders", function()
  run("shop", "create table added (id integer primary key, flavour text)")
  eq(nil, sync(), "sync")
  assert(exists("Shop/shop.db/main/added.table"), "added")
  assert(read("Shop/shop.db/main/added.table"):find("flavour", 1, true), "columns")
  eq("444", mode("Shop/shop.db/main/added.table"), "read-only")
  eq("555", mode("Shop/shop.db/main"), "still read-only")
  -- Not listed before: :DbearSync doesn't connect to it.
  eq({}, ls("Staging/app"), "unlisted databases stay unlisted")
  run("shop", "drop table added")
  sync()
  eq(false, exists("Shop/shop.db/main/added.table"), "stale file removed")
  -- A renamed connection keeps its folder's contents (moved, not listed again).
  store("update connections set name = 'Shop Renamed' where id = 'shop'")
  calls = {}
  sync("")
  eq(false, vim.tbl_contains(ls(""), "Shop"), "old name gone")
  -- "shop" takes the plain name now (and moves back to shop%~2 below).
  assert(vim.tbl_contains(ls(""), "shop"), vim.inspect(ls("")))
  assert(exists("Shop Renamed/shop.db/main/users.table"), "moved: " .. vim.inspect(ls("")))
  -- A connection named like another one's old name.
  store("update connections set name = 'Shop' where id = 'shop'")
  sync("")
  assert(exists("Shop/shop.db/main/users.table"), "renamed back")
  eq({ "%2Ehidden", ".dbear-mirror.json", "Replica", "Shop", "Staging", "a%2Fb%3A c", "shop%~2" }, ls(""), "names swapped back")
  eq("other", manifest.lookup("shop%~2").conn.id, "mapped")
  -- A removed connection's folder goes.
  store("delete from connections where id = 'dot'")
  sync("")
  eq(false, exists("%2Ehidden"), "removed")
  eq(nil, manifest.connection("dot"), "forgotten")
end)

test("case-colliding, odd and long names map to their exact tables", function()
  local long = string.rep("x", 250)
  local nfd = "e\204\129tude" -- e + combining acute: the same file name as "étude" on macOS
  run("other", table.concat({
    'create table "Étude" (a int)',
    'create table "étude" (b int)',
    'create table "' .. nfd .. '" (c int)',
    'create table "a/b:c" (d int)',
    'create table ".hid" (e int)',
    'create table "" (f int)',
    'create table "100%" (g int)',
    'create table "' .. long .. '" (h int)',
    'create table "tab\tname" (i int)',
  }, "; "))
  eq(nil, sync("other"), "sync")
  local files = ls("shop%~2/other.db/main")
  local by_table = {}
  for _, f in ipairs(files) do
    local h = require("dbear.tree.sync").parse_header(read("shop%~2/other.db/main/" .. f):match("^[^\n]*"))
    by_table[h.table] = f
    eq(f, manifest.lookup("shop%~2/other.db/main/" .. f).table.file, "manifest maps " .. f)
    assert(#f <= 255, "length " .. #f)
  end
  eq(9, #files, "one file each: " .. vim.inspect(files))
  assert(by_table["Étude"] and by_table["étude"] and by_table[nfd], "all three: " .. vim.inspect(by_table))
  eq("a%2Fb%3Ac.table", by_table["a/b:c"], "slash and colon")
  eq("%2Ehid.table", by_table[".hid"], "leading dot")
  eq("%-.table", by_table[""], "empty")
  eq("100%25.table", by_table["100%"], "percent")
  eq("tab%09name.table", by_table["tab\tname"], "control character")
  assert(by_table[long]:find("%+", 1, true), "cut, marked: " .. by_table[long])
  for name, f in pairs(by_table) do
    local back = names.unescape(f:gsub("%.table$", ""))
    if name ~= long then
      -- Case collisions are numbered; the name itself still decodes.
      eq(name, back, "unescape " .. f)
    else
      eq(nil, back, "a cut name isn't decoded")
    end
  end
  -- The second of two case-colliding names is numbered; which one is first is byte order.
  -- In byte order the decomposed one comes first and keeps its name; "étude" collides with
  -- "Étude" by case; "Étude" with the decomposed one where normalization doesn't count (macOS).
  eq(nfd .. ".table", by_table[nfd], "first keeps its name")
  assert(by_table["étude"]:match("^étude%%~%d%.table$"), "case collision numbered: " .. by_table["étude"])
  assert(by_table["Étude"]:match("^Étude"), by_table["Étude"])
  -- Opening each one gives its own table.
  local st = rows_buffer("shop%~2/other.db/main/" .. by_table["étude"])
  eq("étude", st.table or st.spec.table, "étude opened")
  eq("b", st.columns[1].name, "its column")
  st = rows_buffer("shop%~2/other.db/main/" .. by_table[nfd])
  eq("c", st.columns[1].name, "the decomposed one")
  st = rows_buffer("shop%~2/other.db/main/" .. by_table[long])
  eq("h", st.columns[1].name, "the long one")
end)

test("opening a table file gives the editable rows buffer; :w changes the database, not the file", function()
  local file = mirror .. "/Shop/shop.db/main/users.table"
  local before = read("Shop/shop.db/main/users.table")
  local st = rows_buffer("Shop/shop.db/main/users.table")
  eq(file, bufname(), "the file's name")
  eq("dbear_table", vim.bo.filetype, "rows buffer")
  eq("acwrite", vim.bo.buftype, "acwrite")
  assert(shown()[1]:find("Ana", 1, true), vim.inspect(shown()))
  local l = vim.api.nvim_buf_get_lines(0, 0, 1, false)[1]
  vim.api.nvim_win_set_cursor(0, { 1, l:find("Ana", 1, true) - 1 })
  keys("ciwAnnie<Esc>")
  vim.cmd("write")
  wait("confirmation", function() return st.confirm_win ~= nil end)
  local sql = vim.api.nvim_buf_get_lines(vim.api.nvim_win_get_buf(st.confirm_win), 2, -1, false)
  eq({ [[UPDATE "main"."users" SET "name" = 'Annie' WHERE "id" = 1;]] }, sql, "the SQL")
  keys("y")
  wait("saved", function() return not st.saving and not st.loading end)
  eq({ { "Annie" } }, run("shop", "select name from users where id = 1"), "in the database")
  eq(before, read("Shop/shop.db/main/users.table"), "the file is untouched")
  eq(false, vim.bo.modified, "clean")
  -- Writable while its buffer is loaded (Vim refuses :w otherwise), read-only again after.
  eq("644", mode("Shop/shop.db/main/users.table"), "writable while open")
  eq("555", mode("Shop/shop.db/main"), "its folder stays read-only")
  sync("shop")
  eq("644", mode("Shop/shop.db/main/users.table"), "a sync keeps it so")
  vim.cmd("bwipeout!")
  eq("444", mode("Shop/shop.db/main/users.table"), "read-only once closed")
end)

test("- goes to the table's folder; gf's filtered view goes back there too", function()
  rows_buffer("Shop/shop.db/main/orders.table")
  keys("-")
  eq(mirror .. "/Shop/shop.db/main", bufname(), "the folder")
  rows_buffer("Shop/shop.db/main/orders.table")
  local line = vim.api.nvim_buf_get_lines(0, 0, 1, false)[1]
  local cells = require("dbear.browse.cells")
  local p = cells.parse(line, #state().columns)
  vim.api.nvim_win_set_cursor(0, { 1, cells.position(line, p, 2, 0) })
  keys("gf")
  wait("followed", function()
    local st = state()
    return vim.api.nvim_buf_get_name(0):find("^dbear://") and st and st.ready
  end)
  assert(vim.api.nvim_buf_get_name(0):find("users?where=", 1, true), vim.api.nvim_buf_get_name(0))
  keys("-")
  eq(mirror .. "/Shop/shop.db/main", bufname(), "the mirror folder")
end)

test(":DbearOpen <connection> opens its folder", function()
  vim.cmd("DbearOpen Staging")
  wait("folder", function() return bufname() == mirror .. "/Staging" end)
  wait("idle", idle)
  vim.cmd("DbearOpen other")
  wait("folder", function() return bufname() == mirror .. "/shop%~2" end)
end)

test("the old dbear:// tree still works (:DbearOpen!)", function()
  vim.cmd("DbearOpen!")
  wait("root", function() return vim.api.nvim_buf_get_name(0) == "dbear://" and vim.api.nvim_buf_get_lines(0, 0, 1, false)[1] ~= "Loading…" end)
  assert(vim.api.nvim_buf_get_lines(0, 0, -1, false)[1]:find("Shop"), "connections listed")
  vim.cmd("DbearOpen shop/main.users")
  wait("rows", function() return state() and state().ready end)
  keys("-")
  wait("list", function() return vim.api.nvim_buf_get_name(0) == "dbear://shop/" end)
end)

test("read-only on disk: files and folders can't be changed, removed or added", function()
  local file = mirror .. "/Shop/shop.db/main/users.table"
  eq(nil, io.open(file, "w"), "can't write")
  eq(nil, (os.remove(file)), "can't delete")
  eq(nil, (os.rename(file, file .. ".x")), "can't rename")
  eq(nil, io.open(mirror .. "/Shop/shop.db/main/new.table", "w"), "can't add")
  eq(nil, (os.remove(mirror .. "/Shop/shop.db/main")), "can't remove a folder")
  eq(nil, (uv.fs_mkdir(mirror .. "/new", 448)), "can't add at the root")
  assert(exists("Shop/shop.db/main/users.table"), "intact")
end)

test("the root guard: nothing outside the root is touched", function()
  local victim = outside .. "/victim.txt"
  local f = assert(io.open(victim, "w"))
  f:write("keep")
  f:close()
  uv.fs_chmod(victim, tonumber("644", 8))
  for _, bad in ipairs({ victim, outside, mirror, mirror .. "/../outside/victim.txt", mirror .. "/Shop/../../outside", "/", "relative/path", mirror .. "x/y" }) do
    for _, op in ipairs({ "remove", "chmod" }) do
      local ok, err = pcall(fs[op], bad, tonumber("777", 8))
      if bad ~= mirror or op ~= "chmod" then
        assert(not ok and tostring(err):find("refusing", 1, true), op .. " " .. bad .. ": " .. tostring(err))
      end
    end
  end
  -- A symlink inside the mirror pointing outside: a folder through it is refused, the link
  -- itself is only ever unlinked (sync removes it as stale), the target is untouched.
  local main = mirror .. "/Shop/shop.db/main"
  fs.with_writable(main, function()
    assert(uv.fs_symlink(outside, main .. "/link"))
    assert(uv.fs_symlink(victim, main .. "/evil.table"))
  end)
  local ok, err = pcall(fs.remove, main .. "/link/victim.txt")
  assert(not ok and tostring(err):find("refusing", 1, true), "through a link: " .. tostring(err))
  ok = pcall(fs.write, main .. "/link/new.txt", "x")
  assert(not ok, "no write through a link")
  sync("shop", true)
  eq(false, exists("Shop/shop.db/main/link"), "the link is removed")
  eq(false, exists("Shop/shop.db/main/evil.table"), "the file link is removed")
  eq("keep", fs.read(victim), "the target is untouched")
  eq("644", string.format("%o", bit.band(uv.fs_stat(victim).mode, tonumber("777", 8))), "its mode too")
  eq("keep", fs.read(victim), "")
  assert(uv.fs_stat(outside), "the outside folder is still there")
  -- A folder that isn't a mirror (not empty, no manifest) is refused as a root.
  ok, err = pcall(function()
    local saved = fs.root()
    fs.set_root(outside)
    local res = { pcall(fs.ensure_root) }
    fs.set_root(saved)
    assert(not res[1], "refused")
    error(res[2], 0)
  end)
  assert(tostring(err):find("isn't a dbear mirror", 1, true), tostring(err))
end)

test(":DbearSync! rebuilds: rewrites files and resets permissions", function()
  local f = mirror .. "/Shop/shop.db/main/users.table"
  uv.fs_chmod(mirror .. "/Shop/shop.db/main", tonumber("755", 8))
  uv.fs_chmod(f, tonumber("644", 8))
  local h = assert(io.open(f, "a"))
  h:write("garbage\n")
  h:close()
  eq(nil, sync("", true), "rebuild")
  eq("444", mode("Shop/shop.db/main/users.table"), "file mode")
  eq("555", mode("Shop/shop.db/main"), "folder mode")
  eq(nil, read("Shop/shop.db/main/users.table"):find("garbage", 1, true), "rewritten")
  assert(exists("Staging/app_test/public/users.table"), "listed databases relisted")
  eq({}, ls("Staging/app"), "unlisted ones still not listed")
end)

test("URL connections aren't mirrored", function()
  local before = ls("")
  tree.sync("sqlite:" .. os.getenv("DBEAR_TEST_SHOP"), false)
  assert(messages[#messages].msg:find("aren't mirrored", 1, true), messages[#messages].msg)
  -- Using one doesn't add it either.
  run("sqlite:" .. os.getenv("DBEAR_TEST_SHOP"), "select 1")
  sync("")
  eq(before, ls(""), "no new folder")
  for _, c in ipairs(manifest.get().connections) do
    assert(not c.id:find("^url:"), c.id)
  end
end)

test(":DbearFind and :DbearGrep without telescope", function()
  local picked
  vim.ui.select = function(items, _, cb)
    picked = items
    cb("Shop/shop.db/main/users.table")
  end
  vim.cmd("DbearFind")
  assert(vim.tbl_contains(picked, "Staging/app_test/public/users.table"), vim.inspect(picked))
  wait("rows", function() return state() and state().ready end)
  eq(mirror .. "/Shop/shop.db/main/users.table", bufname(), "opened")
  vim.cmd("DbearGrep email")
  local qf = vim.fn.getqflist()
  assert(#qf >= 2, "matches: " .. #qf)
  local found = false
  for _, item in ipairs(qf) do
    if bufname(item.bufnr) == mirror .. "/Shop/shop.db/main/users.table" then
      found = true
    end
  end
  assert(found, "users.table in the quickfix list")
end)

test("relist = never: a folder listed before isn't listed again in a new session", function()
  require("dbear.tree.sync").listed = {}
  dbear.config.tree.relist = "never"
  calls = {}
  enter("Shop")
  enter("Staging/app_test")
  eq({}, methods(), "nothing listed")
  dbear.config.tree.relist = "session"
  enter("Shop")
  eq({ "schemas.list shop", "columns.list shop" }, methods(), "listed in a new session")
end)

-- MARK: renaming connections (without oil: mv, :DbearRename)

local function saved()
  local err, res = dbear.rpc():request_sync("connections.list", nil, 10000)
  assert(not err, vim.inspect(err))
  local out = {}
  for _, c in ipairs(res.connections) do
    out[c.id] = c.name
  end
  return out
end

local function said(text)
  for _, m in ipairs(messages) do
    if m.msg:find(text, 1, true) then
      return true
    end
  end
  return false
end

local W, R = tonumber("755", 8), tonumber("555", 8)

--- `mv` at the root by someone who made it writable first (a plain `mv` is refused).
local function forced(fn)
  uv.fs_chmod(mirror, W)
  local ok, err = pcall(fn)
  uv.fs_chmod(mirror, R)
  assert(ok, err)
end

local function forced_mv(from, to)
  forced(function()
    uv.fs_chmod(mirror .. "/" .. from, W)
    assert(os.rename(mirror .. "/" .. from, mirror .. "/" .. to))
    uv.fs_chmod(mirror .. "/" .. to, R)
  end)
end

test("a plain mv at the root is refused; a forced one is understood as a rename", function()
  sync("")
  local ident = fs.identity(mirror .. "/Shop")
  local ok = os.rename(mirror .. "/Shop", mirror .. "/Moved")
  eq(nil, ok, "plain mv: permission denied")
  eq("Shop", saved().shop, "store unchanged")
  -- A table buffer of Shop with a pending edit.
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/Shop/shop.db/main/users.table"))
  wait("rows", function() return state() and state().ready end)
  local rows = vim.api.nvim_get_current_buf()
  vim.api.nvim_win_set_cursor(0, { 2, 0 })
  vim.cmd("normal! $")
  vim.cmd("enew")
  forced_mv("Shop", "Moved")
  messages = {}
  sync("")
  eq("Moved", saved().shop, "store renamed")
  assert(said("renamed “Shop” to “Moved”"), vim.inspect(messages))
  eq(ident, fs.identity(mirror .. "/Moved"), "the same folder")
  eq("shop", manifest.lookup("Moved").conn.id, "manifest")
  assert(exists("Moved/shop.db/main/users.table"), "contents kept")
  eq(false, vim.tbl_contains(ls(""), "Shop"), "no Shop folder")
  eq(mirror .. "/Moved/shop.db/main/users.table", bufname(rows), "the rows buffer moved with it")
  eq("644", mode("Moved/shop.db/main/users.table"), "still writable while loaded")
  -- With the collision gone, "shop" (id other) lost its number.
  eq("other", manifest.lookup("shop").conn.id, "shop un-numbered")
  -- Listing again changes nothing.
  local before = ls("")
  messages = {}
  sync("")
  eq(before, ls(""), "no-op")
  eq(1, #messages, "nothing else said: " .. vim.inspect(messages))
  assert(messages[1].msg:find("^dbear: synced"), messages[1].msg)
  vim.cmd("bwipeout! " .. rows)
  eq("444", mode("Moved/shop.db/main/users.table"), "read-only once closed")
end)

test(":DbearRename renames from a folder or a table buffer; names the store refuses are put back", function()
  -- From a table buffer, with a name.
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/Moved/shop.db/main/users.table"))
  wait("rows", function() return state() and state().ready end)
  vim.cmd("DbearRename Shop")
  wait("renamed", function() return saved().shop == "Shop" and idle() and exists("Shop") end)
  eq(mirror .. "/Shop/shop.db/main/users.table", bufname(), "the buffer followed")
  assert(said("renamed “Moved” to “Shop”"), vim.inspect(messages))
  eq("other", manifest.lookup("shop%~2").conn.id, "shop numbered again")
  -- From a folder, asked for (vim.ui.input).
  enter("shop%~2")
  local input = vim.ui.input
  local asked
  vim.ui.input = function(opts, cb)
    asked = opts
    cb("Other shop")
  end
  vim.cmd("DbearRename")
  wait("asked", function() return asked ~= nil end)
  vim.ui.input = input
  eq("shop", asked.default, "asks with the current name")
  wait("renamed", function() return saved().other == "Other shop" and idle() and exists("Other shop") end)
  -- `%-` (the empty name) gets the store's default name (the file's), as when saving.
  forced_mv("Other shop", "%-")
  sync("")
  eq("other.db", saved().other, "the default name")
  assert(exists("other.db"), "its folder")
  -- A cut name can't be read back: put back.
  forced_mv("other.db", "x%+0123456789")
  messages = {}
  sync("")
  eq("other.db", saved().other, "unchanged")
  assert(exists("other.db") and not exists("x%+0123456789"), "put back")
  assert(said("was cut"), vim.inspect(messages))
  -- A connection the store lost meanwhile (deleted by another app): the rename fails, the folder
  -- goes with the connection.
  forced_mv("other.db", "Gone")
  store("delete from connections where id = 'other'")
  messages = {}
  sync("")
  assert(said("can't rename “other.db” to “Gone”"), vim.inspect(messages))
  eq(nil, saved().other, "not re-created")
  eq(false, exists("Gone") or exists("other.db"), "its folder goes")
  eq(nil, manifest.connection("other"), "and its manifest entry")
  -- Put it back as it was (tree_plugins_spec.lua uses the same store).
  store("insert into connections (id, position, name, kind, database, ssl_mode) values ('other', 2, 'shop', 'sqlite', '"
    .. vim.fs.dirname(os.getenv("DBEAR_TEST_SHOP")) .. "/other.db', 'prefer')")
  sync("")
  eq("other", manifest.lookup("shop%~2").conn.id, "back")
end)

test("a connection folder deleted, and junk created, from outside: put back, store untouched", function()
  local before_store = saved()
  forced(function()
    assert(uv.fs_rmdir(mirror .. "/Replica")) -- empty: its listing failed
    assert(uv.fs_mkdir(mirror .. "/junk", W))
    local f = assert(io.open(mirror .. "/note.txt", "w"))
    f:close()
  end)
  messages = {}
  sync("")
  eq(before_store, saved(), "store unchanged")
  assert(exists("Replica"), "Replica's folder is back")
  eq(false, exists("junk") or exists("note.txt"), "junk removed")
  assert(said("“Replica” is still saved"), vim.inspect(messages))
  assert(said("removed “junk”, “note.txt”"), vim.inspect(messages))
  eq("555", mode(""), "root read-only")
end)

test("tree.rename = false: a renamed folder goes back", function()
  dbear.config.tree.rename = false
  forced_mv("Staging", "Stage")
  messages = {}
  sync("")
  dbear.config.tree.rename = true
  eq("Staging", saved()["staging-pg"], "store unchanged")
  assert(exists("Staging") and not exists("Stage"), "put back")
  assert(said("tree.rename = false"), vim.inspect(messages))
end)

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
