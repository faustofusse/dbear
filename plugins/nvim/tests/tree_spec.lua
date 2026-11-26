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
local progressed = {} -- Progress events (see below)
local seq = 0 -- orders messages and progress events
vim.notify = function(msg, level)
  seq = seq + 1
  messages[#messages + 1] = { msg = msg, level = level, seq = seq }
end

local function test(name, fn)
  messages = {}
  progressed = {}
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

-- Every request the plugin sends. With `slow` > 0, answers arrive that many ms late.
local calls = {}
local slow = 0
local client = dbear.rpc()
local request = client.request
client.request = function(self, method, params, cb, fast)
  calls[#calls + 1] = { method = method, connection = params and params.connection, database = params and params.database }
  if slow > 0 and cb then
    local answer, ms = cb, slow
    cb = function(...)
      local args = { n = select("#", ...), ... }
      vim.defer_fn(function() answer(unpack(args, 1, args.n)) end, ms)
    end
  end
  return request(self, method, params, cb, fast)
end

-- Progress messages (`nvim_echo` with kind = "progress"), where this Neovim has them.
local progress = require("dbear.tree.progress")
local has_progress = vim.fn.exists("##Progress") == 1
if has_progress then
  vim.api.nvim_create_autocmd("Progress", {
    pattern = "dbear",
    callback = function(ev)
      local text = ev.data.text
      if type(text) == "table" then -- chunks
        text = table.concat(vim.tbl_map(function(c) return type(c) == "table" and c[1] or c end, text))
      end
      seq = seq + 1
      progressed[#progressed + 1] = { id = ev.data.id, status = ev.data.status, text = text, seq = seq }
    end,
  })
end
local function methods()
  return vim.tbl_map(function(c) return c.method .. (c.connection and " " .. c.connection or "") end, calls)
end

local function idle()
  return tree.idle()
end

local function ls(rel)
  local out = {}
  for name in fs.list(fs.root() .. (rel ~= "" and "/" .. rel or "")) do
    out[#out + 1] = name
  end
  table.sort(out)
  return out
end

local function exists(rel)
  return uv.fs_lstat(fs.root() .. "/" .. rel) ~= nil
end

local function mode(rel)
  return string.format("%o", bit.band(uv.fs_lstat(fs.root() .. (rel ~= "" and "/" .. rel or "")).mode, tonumber("777", 8)))
end

local function read(rel)
  return fs.read(fs.root() .. "/" .. rel)
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
  vim.cmd.edit(vim.fn.fnameescape(fs.root() .. (rel ~= "" and "/" .. rel or "")))
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
  vim.cmd.edit(vim.fn.fnameescape(fs.root() .. "/" .. rel))
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
  eq({ "big", "named", "orders", "users" }, ls("Shop/shop.db/main"), "tables and views")
  eq("444", mode("Shop/shop.db/main/users"), "file read-only")
  eq("555", mode("Shop/shop.db/main"), "folder read-only")
  -- Entering it again in the same session doesn't list again.
  calls = {}
  enter("Shop")
  enter("Shop/shop.db/main")
  eq({}, methods(), "listed once per session")
end)

test("table files hold the target and the column list", function()
  local text = read("Shop/shop.db/main/users")
  local lines = vim.split(text, "\n", { plain = true })
  local h = require("dbear.tree.sync").parse_header(lines[1])
  eq({ connection = "shop", database = os.getenv("DBEAR_TEST_SHOP"), browses = false, schema = "main", table = "users", kind = "table" }, h, "header")
  assert(text:find("\nid     integer  PK  NOT NULL\n", 1, true), text)
  assert(text:find("\nemail  text         NULL\n", 1, true), text)
  local view = read("Shop/shop.db/main/named")
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
  assert(vim.tbl_contains(ls("Staging/app_test/public"), "users"), vim.inspect(ls("Staging/app_test/public")))
  eq({}, ls("Staging/app"), "other databases still not listed")
  local h = require("dbear.tree.sync").parse_header(read("Staging/app_test/public/users"):match("^[^\n]*"))
  eq({ "staging-pg", "app_test", true }, { h.connection, h.database, h.browses }, "header")
end)

--- Messages at `level` that contain `text`.
local function notified(level, text)
  local out = {}
  for _, m in ipairs(messages) do
    if m.level == level and (not text or m.msg:find(text, 1, true)) then
      out[#out + 1] = m.msg
    end
  end
  return out
end

--- Extmarks (any namespace) in the folder buffers: the mirror never adds any.
local function folder_marks()
  local n = 0
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and tree.folder_of(buf) then
      n = n + #vim.api.nvim_buf_get_extmarks(buf, -1, 0, -1, {})
    end
  end
  return n
end

test("an unreachable server: the error is notified once, nothing is created", function()
  calls = {}
  enter("Replica")
  eq({ "databases.list prod-replica" }, methods(), "tried")
  eq({}, ls("Replica"), "nothing")
  local errs = notified(vim.log.levels.ERROR)
  eq(1, #errs, "one error: " .. vim.inspect(messages))
  assert(errs[1]:find("could not connect", 1, true) and errs[1]:find("(:DbearSync Replica retries)", 1, true), errs[1])
  assert(manifest.connection("prod-replica").error, "recorded")
  -- Entering the same folder again (focus, :edit, a redraw) says nothing more.
  vim.cmd("doautocmd BufEnter")
  enter("Replica")
  eq(1, #messages, "no repeat: " .. vim.inspect(messages))
  -- Coming back to it (it isn't listed again): one warning with the hint, once per failure.
  local buf = vim.api.nvim_get_current_buf()
  enter("")
  vim.cmd("buffer " .. buf) -- the same buffer, as oil does
  local warns = notified(vim.log.levels.WARN)
  eq(1, #warns, "one warning: " .. vim.inspect(messages))
  assert(warns[1]:find("the last listing of databases of Replica failed", 1, true) and warns[1]:find("(:DbearSync Replica retries)", 1, true), warns[1])
  enter("")
  vim.cmd("buffer " .. buf)
  eq(1, #notified(vim.log.levels.WARN), "not again for the same failure")
  eq(0, folder_marks(), "no extmarks")
  eq(nil, vim.b.dbear_tree_status, "no status variable")
  eq({}, progressed, "a quick failure shows no progress")
end)

test("a slow listing shows one progress item, which ends; nothing stays", function()
  slow = 600
  enter("Staging/postgres")
  slow = 0
  assert(#ls("Staging/postgres") > 0, "listed")
  eq(false, progress.active(), "nothing pending")
  eq({}, notified(vim.log.levels.INFO), "no messages: " .. vim.inspect(messages))
  if has_progress then
    eq("echo", progress.backend(), "this Neovim's progress messages")
    assert(#progressed >= 2, vim.inspect(progressed))
    local id = progressed[1].id
    for _, p in ipairs(progressed) do
      eq(id, p.id, "one item")
    end
    eq("running", progressed[1].status, "first")
    assert(progressed[1].text:find("listing Staging / postgres…", 1, true), progressed[1].text)
    eq("success", progressed[#progressed].status, "ended")
    assert(progressed[#progressed].text:find("listed Staging / postgres", 1, true), progressed[#progressed].text)
  else
    eq("notify", progress.backend(), "no progress messages in this Neovim")
  end
  eq(0, folder_marks(), "no extmarks")
end)

test("an instant listing shows no progress", function()
  local done
  require("dbear.tree.sync").root(nil, function(e) done = { e } end)
  wait("root", function() return done ~= nil end)
  eq(nil, done[1], "listed")
  vim.wait(progress.delay + 150)
  eq({}, progressed, "no progress item")
  eq({}, messages, "no message")
  eq(false, progress.active(), "nothing pending")
end)

test("a slow failure ends its progress and notifies the error once; coming back warns once", function()
  slow = 600
  local err = sync("Replica")
  slow = 0
  assert(err, "fails")
  eq(false, progress.active(), "nothing pending")
  local errs = notified(vim.log.levels.ERROR)
  eq(1, #errs, "one error: " .. vim.inspect(messages))
  assert(errs[1]:find("(:DbearSync Replica retries)", 1, true), errs[1])
  if has_progress then
    assert(#progressed >= 2, vim.inspect(progressed))
    eq("running", progressed[1].status, "first")
    eq("failed", progressed[#progressed].status, "ended as failed")
    assert(progressed[#progressed].text:find("couldn't list databases of Replica", 1, true), progressed[#progressed].text)
    local err_seq
    for _, m in ipairs(messages) do
      if m.level == vim.log.levels.ERROR then
        err_seq = m.seq
      end
    end
    assert(progressed[#progressed].seq < err_seq, "the progress ends before the error, which stays on screen")
  end
  -- Coming back to the folder (it isn't listed again): one warning with the hint, then quiet.
  messages = {}
  calls = {}
  enter("Replica")
  eq({}, methods(), "not retried by entering")
  local warns = notified(vim.log.levels.WARN)
  eq(1, #warns, "one warning: " .. vim.inspect(messages))
  assert(warns[1]:find("the last listing of databases of Replica failed", 1, true) and warns[1]:find("(:DbearSync Replica retries)", 1, true), warns[1])
  vim.cmd("doautocmd BufEnter")
  enter("Replica")
  local buf = vim.api.nvim_get_current_buf()
  vim.cmd("enew")
  vim.cmd("buffer " .. buf)
  eq(1, #messages, "no repeats: " .. vim.inspect(messages))
  eq(false, progress.active(), "nothing pending")
  eq(0, folder_marks(), "no extmarks")
end)

test("fidget's progress API when it's installed; else one message when a listing starts", function()
  local handles = {}
  package.loaded["fidget.progress"] = nil
  package.preload["fidget.progress"] = function()
    return {
      handle = {
        create = function(msg)
          local h = { created = vim.deepcopy(msg), reports = {}, finished = 0 }
          function h:report(p) table.insert(self.reports, p) end
          function h:finish() self.finished = self.finished + 1 end
          handles[#handles + 1] = h
          return h
        end,
      },
    }
  end
  local ok, err = pcall(function()
    eq("fidget", progress.backend(), "fidget first")
    require("dbear.tree.sync").listed[require("dbear.tree.sync").db_key("staging-pg", "postgres")] = nil
    vim.cmd("enew")
    slow = 600
    enter("Staging/postgres")
    slow = 0
    eq(1, #handles, "one handle")
    eq("listing Staging / postgres…", handles[1].created.message, "its message")
    eq(1, handles[1].finished, "finished")
    eq("listed Staging / postgres", handles[1].reports[#handles[1].reports].message, "done")
    eq({}, progressed, "not also a progress message")
    eq({}, messages, "no other message")
  end)
  package.preload["fidget.progress"] = nil
  package.loaded["fidget.progress"] = nil
  assert(ok, err)
  -- Without fidget nor progress messages (Neovim 0.10, 0.11): one INFO message when it starts.
  progress.force = "notify"
  ok, err = pcall(function()
    require("dbear.tree.sync").listed[require("dbear.tree.sync").db_key("staging-pg", "postgres")] = nil
    vim.cmd("enew")
    slow = 600
    enter("Staging/postgres")
    slow = 0
    eq({ "dbear: listing Staging / postgres…" }, vim.tbl_map(function(m) return m.msg end, messages), "one message")
    eq(vim.log.levels.INFO, messages[1].level, "INFO")
    eq({}, progressed, "no progress message")
  end)
  progress.force = nil
  slow = 0
  assert(ok, err)
end)

test("a failed listing keeps the previous tree", function()
  local before = ls("Shop/shop.db/main")
  store("update connections set database = '/nonexistent/dir/shop.db' where id = 'shop'")
  local err = sync("shop")
  assert(err, "fails")
  eq(before, ls("Shop/shop.db/main"), "files kept")
  assert(read("Shop/shop.db/main/users"):find("email", 1, true), "content kept")
  eq(1, #notified(vim.log.levels.ERROR, "(:DbearSync Shop retries)"), "error once: " .. vim.inspect(messages))
  messages = {}
  enter("Shop/shop.db/main")
  local warns = notified(vim.log.levels.WARN)
  eq(1, #warns, "coming back warns: " .. vim.inspect(messages))
  assert(warns[1]:find("the last listing of Shop failed", 1, true) and warns[1]:find("(:DbearSync Shop retries)", 1, true), warns[1])
  store("update connections set database = '" .. os.getenv("DBEAR_TEST_SHOP") .. "' where id = 'shop'")
  eq(nil, sync("shop"), "works again")
  messages = {}
  vim.cmd("enew")
  enter("Shop/shop.db/main")
  eq({}, messages, "nothing once it works")
end)

test("sync adds and removes tables, renames and removes connections, on read-only folders", function()
  run("shop", "create table added (id integer primary key, flavour text)")
  eq(nil, sync(), "sync")
  assert(exists("Shop/shop.db/main/added"), "added")
  assert(read("Shop/shop.db/main/added"):find("flavour", 1, true), "columns")
  eq("444", mode("Shop/shop.db/main/added"), "read-only")
  eq("555", mode("Shop/shop.db/main"), "still read-only")
  -- Not listed before: :DbearSync doesn't connect to it.
  eq({}, ls("Staging/app"), "unlisted databases stay unlisted")
  run("shop", "drop table added")
  sync()
  eq(false, exists("Shop/shop.db/main/added"), "stale file removed")
  -- A renamed connection keeps its folder's contents (moved, not listed again).
  store("update connections set name = 'Shop Renamed' where id = 'shop'")
  calls = {}
  sync("")
  eq(false, vim.tbl_contains(ls(""), "Shop"), "old name gone")
  -- "shop" takes the plain name now (and moves back to shop%~2 below).
  assert(vim.tbl_contains(ls(""), "shop"), vim.inspect(ls("")))
  assert(exists("Shop Renamed/shop.db/main/users"), "moved: " .. vim.inspect(ls("")))
  -- A connection named like another one's old name.
  store("update connections set name = 'Shop' where id = 'shop'")
  sync("")
  assert(exists("Shop/shop.db/main/users"), "renamed back")
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
  eq("a%2Fb%3Ac", by_table["a/b:c"], "slash and colon")
  eq("%2Ehid", by_table[".hid"], "leading dot")
  eq("%-", by_table[""], "empty")
  eq("100%25", by_table["100%"], "percent")
  eq("tab%09name", by_table["tab\tname"], "control character")
  assert(by_table[long]:find("%+", 1, true), "cut, marked: " .. by_table[long])
  for name, f in pairs(by_table) do
    local back = names.unescape(f)
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
  eq(nfd, by_table[nfd], "first keeps its name")
  assert(by_table["étude"]:match("^étude%%~%d$"), "case collision numbered: " .. by_table["étude"])
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

test("names with dots and extensions are file names as they are; each opens as its rows, whatever the name", function()
  vim.cmd("filetype on") -- detection by name would turn these into sql, markdown, json, make…
  local odd = { "users.sql", "README.md", "data.json", "Makefile", "a.tar.gz", "x.table", "y.view", "trail.", "trail ", ".dbear-mirror.json", "init.vim" }
  local sql = {}
  for _, n in ipairs(odd) do
    sql[#sql + 1] = string.format([[create table "%s" (id integer primary key, v text); insert into "%s" values (1, 'vim: set ts=3 ft=lua :')]], n, n)
  end
  run("other", table.concat(sql, "; "))
  eq(nil, sync("other"), "sync")
  local dir = "shop%~2/other.db/main/"
  local want = {
    ["users.sql"] = "users.sql", ["README.md"] = "README.md", ["data.json"] = "data.json", Makefile = "Makefile",
    ["a.tar.gz"] = "a.tar.gz", ["x.table"] = "x.table", ["y.view"] = "y.view", ["trail."] = "trail%2E",
    ["trail "] = "trail%20", [".dbear-mirror.json"] = "%2Edbear-mirror.json", ["init.vim"] = "init.vim",
  }
  for _, n in ipairs(odd) do
    local f = want[n]
    assert(exists(dir .. f), n .. " as " .. f .. ": " .. vim.inspect(ls(dir)))
    eq("444", mode(dir .. f), "read-only")
    eq(n, manifest.lookup(dir .. f).table.name, "manifest maps " .. f)
    eq(n, require("dbear.tree.sync").parse_header(read(dir .. f):match("^[^\n]*")).table, "header of " .. f)
    local st = rows_buffer(dir .. f)
    eq(n, st.spec.table, "opens " .. n)
    eq("dbear_table", vim.bo.filetype, "no filetype from the name " .. f)
    eq("acwrite", vim.bo.buftype, "rows buffer " .. f)
    eq(false, vim.bo.swapfile, "no swap file " .. f)
    eq(false, vim.bo.modeline, "no modelines " .. f)
    assert(shown()[1]:find("vim: set", 1, true), vim.inspect(shown()))
    eq(8, vim.bo.tabstop, "the row's modeline isn't applied")
    vim.cmd("edit!")
    wait("reloaded", function() return state() and state().ready and not state().loading end)
    eq("dbear_table", vim.bo.filetype, ":e! keeps it a rows buffer " .. f)
    eq(8, vim.bo.tabstop, "still no modeline")
  end
  eq({}, notified(vim.log.levels.ERROR), "no errors")
  -- `x.table` is the table named so, not "x" with the old suffix.
  eq(nil, manifest.lookup(dir .. "x"), "no table x")
  run("other", table.concat(vim.tbl_map(function(n) return 'drop table "' .. n .. '"' end, odd), "; "))
  eq(nil, sync("other"), "sync")
  for _, f in pairs(want) do
    eq(false, exists(dir .. f), "removed " .. f)
  end
end)

test("only table files are caught: the manifest and anything else open as plain files", function()
  vim.cmd("filetype on")
  -- The manifest: a normal JSON buffer.
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/" .. fs.MANIFEST))
  eq("", vim.bo.buftype, "a file buffer")
  eq("json", vim.bo.filetype, "detected as JSON")
  eq(nil, state(), "no rows buffer")
  assert(vim.api.nvim_buf_get_lines(0, 0, 1, false)[1]:find('"version"', 1, true), "its text")
  vim.cmd("bwipeout!")
  -- Something four levels down that isn't a table file (no header), and deeper: read as usual.
  local main = mirror .. "/Shop/shop.db/main"
  fs.with_writable(main, function()
    local f = assert(io.open(main .. "/notes.json", "w"))
    f:write('{"a": 1}\n')
    f:close()
  end)
  vim.cmd.edit(vim.fn.fnameescape(main .. "/notes.json"))
  eq(nil, state(), "no rows buffer")
  eq("", vim.bo.buftype, "a file buffer")
  eq("json", vim.bo.filetype, "filetype detected")
  eq({ '{"a": 1}' }, vim.api.nvim_buf_get_lines(0, 0, -1, false), "its text, once")
  eq(false, vim.bo.modified, "not modified")
  eq(mirror .. "/Shop/shop.db/main/notes.json", bufname(), "its name")
  eq(0, vim.fn.undotree().seq_last, "reading it is nothing to undo")
  vim.cmd("bwipeout!")
  vim.cmd.edit(vim.fn.fnameescape(main .. "/missing/deeper.txt"))
  eq(nil, state(), "no rows buffer")
  eq({ "" }, vim.api.nvim_buf_get_lines(0, 0, -1, false), "a new file")
  vim.cmd("bwipeout!")
  -- A symlink to a table file isn't one (the mirror never makes links; sync removes it).
  fs.with_writable(main, function() assert(uv.fs_symlink(main .. "/users", main .. "/alias")) end)
  vim.cmd.edit(vim.fn.fnameescape(main .. "/alias"))
  eq(nil, state(), "a link isn't opened as rows")
  vim.cmd("bwipeout!")
  eq(nil, sync("shop"), "sync")
  eq({ "big", "named", "orders", "users" }, ls("Shop/shop.db/main"), "junk removed")
end)

test("a view opens read-only, with the reason; its kind is in its file", function()
  local text = read("Shop/shop.db/main/named")
  eq("view", require("dbear.tree.sync").parse_header(text:match("^[^\n]*")).kind, "header")
  assert(text:find("\n-- named: view in Shop / shop.db / main\n", 1, true), text)
  local st = rows_buffer("Shop/shop.db/main/named")
  assert(st.read_only and st.read_only ~= "", "read-only")
  eq(false, vim.bo.modifiable, "not modifiable")
  local virt = vim.api.nvim_buf_get_extmarks(0, -1, 0, -1, { details = true })
  local said_it = false
  for _, m in ipairs(virt) do
    for _, chunks in ipairs(m[4].virt_lines or {}) do
      for _, c in ipairs(chunks) do
        said_it = said_it or c[1]:find("read-only: ", 1, true) ~= nil
      end
    end
  end
  assert(said_it, "the reason is shown: " .. st.read_only)
end)

test("opening a table file gives the editable rows buffer; :w changes the database, not the file", function()
  local file = mirror .. "/Shop/shop.db/main/users"
  local before = read("Shop/shop.db/main/users")
  local st = rows_buffer("Shop/shop.db/main/users")
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
  eq(before, read("Shop/shop.db/main/users"), "the file is untouched")
  eq(false, vim.bo.modified, "clean")
  -- Writable while its buffer is loaded (Vim refuses :w otherwise), read-only again after.
  eq("644", mode("Shop/shop.db/main/users"), "writable while open")
  eq("555", mode("Shop/shop.db/main"), "its folder stays read-only")
  sync("shop")
  eq("644", mode("Shop/shop.db/main/users"), "a sync keeps it so")
  vim.cmd("bwipeout!")
  eq("444", mode("Shop/shop.db/main/users"), "read-only once closed")
end)

test("- goes to the table's folder; gf's filtered view goes back there too", function()
  rows_buffer("Shop/shop.db/main/orders")
  keys("-")
  eq(mirror .. "/Shop/shop.db/main", bufname(), "the folder")
  rows_buffer("Shop/shop.db/main/orders")
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
  local file = mirror .. "/Shop/shop.db/main/users"
  eq(nil, io.open(file, "w"), "can't write")
  eq(nil, (os.remove(file)), "can't delete")
  eq(nil, (os.rename(file, file .. ".x")), "can't rename")
  eq(nil, io.open(mirror .. "/Shop/shop.db/main/new", "w"), "can't add")
  eq(nil, (os.remove(mirror .. "/Shop/shop.db/main")), "can't remove a folder")
  eq(nil, (uv.fs_mkdir(mirror .. "/new", 448)), "can't add at the root")
  assert(exists("Shop/shop.db/main/users"), "intact")
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
    assert(uv.fs_symlink(victim, main .. "/evil"))
  end)
  local ok, err = pcall(fs.remove, main .. "/link/victim.txt")
  assert(not ok and tostring(err):find("refusing", 1, true), "through a link: " .. tostring(err))
  ok = pcall(fs.write, main .. "/link/new.txt", "x")
  assert(not ok, "no write through a link")
  sync("shop", true)
  eq(false, exists("Shop/shop.db/main/link"), "the link is removed")
  eq(false, exists("Shop/shop.db/main/evil"), "the file link is removed")
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
  local f = mirror .. "/Shop/shop.db/main/users"
  uv.fs_chmod(mirror .. "/Shop/shop.db/main", tonumber("755", 8))
  uv.fs_chmod(f, tonumber("644", 8))
  local h = assert(io.open(f, "a"))
  h:write("garbage\n")
  h:close()
  eq(nil, sync("", true), "rebuild")
  eq("444", mode("Shop/shop.db/main/users"), "file mode")
  eq("555", mode("Shop/shop.db/main"), "folder mode")
  eq(nil, read("Shop/shop.db/main/users"):find("garbage", 1, true), "rewritten")
  assert(exists("Staging/app_test/public/users"), "listed databases relisted")
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
    cb("Shop/shop.db/main/users")
  end
  vim.cmd("DbearFind")
  assert(vim.tbl_contains(picked, "Staging/app_test/public/users"), vim.inspect(picked))
  wait("rows", function() return state() and state().ready end)
  eq(mirror .. "/Shop/shop.db/main/users", bufname(), "opened")
  vim.cmd("DbearGrep email")
  local qf = vim.fn.getqflist()
  assert(#qf >= 2, "matches: " .. #qf)
  local found = false
  for _, item in ipairs(qf) do
    if bufname(item.bufnr) == mirror .. "/Shop/shop.db/main/users" then
      found = true
    end
  end
  assert(found, "users in the quickfix list")
  -- Only table files: never the manifest (\"ident\" is only there), with rg or with grep.
  local function grep_files(pattern)
    messages = {}
    vim.cmd("cclose")
    vim.cmd("DbearGrep " .. pattern)
    local files = {}
    for _, item in ipairs(vim.fn.getqflist()) do
      local rel = fs.rel(bufname(item.bufnr))
      files[rel] = true
      eq(3, select(2, rel:gsub("/", "")), "a table file: " .. rel)
    end
    return files
  end
  local executable = vim.fn.executable
  for _, tool in ipairs({ "rg", "grep" }) do
    if tool == "grep" then
      vim.fn.executable = function(x) return x ~= "rg" and executable(x) or 0 end
    end
    local ok, err = pcall(function()
      eq({}, grep_files("ident"), tool .. ": nothing from the manifest")
      eq(1, #notified(vim.log.levels.INFO, "no column matches ident"), tool .. ": said so")
      local views = grep_files("view in")
      assert(views["Shop/shop.db/main/named"], tool .. ": a view by its kind: " .. vim.inspect(views))
      eq(nil, views["Shop/shop.db/main/users"], tool .. ": not tables")
      assert(grep_files("browses")["Shop/shop.db/main/users"], tool .. ": headers are searched")
      eq(nil, grep_files("browses")[fs.MANIFEST], tool .. ": the manifest isn't")
    end)
    vim.fn.executable = executable
    assert(ok, err)
  end
  for _, f in ipairs(picked) do
    eq(3, select(2, f:gsub("/", "")), "only table files to pick: " .. f)
    assert(not f:find(fs.MANIFEST, 1, true), f)
  end
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
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/Shop/shop.db/main/users"))
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
  assert(exists("Moved/shop.db/main/users"), "contents kept")
  eq(false, vim.tbl_contains(ls(""), "Shop"), "no Shop folder")
  eq(mirror .. "/Moved/shop.db/main/users", bufname(rows), "the rows buffer moved with it")
  eq("644", mode("Moved/shop.db/main/users"), "still writable while loaded")
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
  eq("444", mode("Moved/shop.db/main/users"), "read-only once closed")
end)

test(":DbearRename renames from a folder or a table buffer; names the store refuses are put back", function()
  -- From a table buffer, with a name.
  vim.cmd.edit(vim.fn.fnameescape(mirror .. "/Moved/shop.db/main/users"))
  wait("rows", function() return state() and state().ready end)
  vim.cmd("DbearRename Shop")
  wait("renamed", function() return saved().shop == "Shop" and idle() and exists("Shop") end)
  eq(mirror .. "/Shop/shop.db/main/users", bufname(), "the buffer followed")
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

-- MARK: migration from the old layout

test("a mirror written with suffixes (manifest v1) is renamed in place at the next listing, offline", function()
  local sync_ = require("dbear.tree.sync")
  local old_root = mirror .. "-v1"
  dbear.config.tree.root = old_root
  tree.attach()
  eq(old_root, fs.root(), "switched")
  sync_.listed = {}
  -- `x` and `x.table` were `x.table` and `x.table.table`: the names chain.
  run("other", [[create table "x" (a int); create table "x.table" (b int); create view "v" as select 1 as one]])
  eq(nil, sync("shop"), "Shop")
  eq(nil, sync("other"), "other")
  enter("Staging")
  enter("Staging/app_test")
  eq({}, ls("Staging/app"), "never listed")
  -- Back to version 1: every table file renamed `<file>.<kind>`, the manifest says so.
  local man = manifest.get()
  local W = tonumber("755", 8)
  local old_files = {}
  for _, c in ipairs(man.connections) do
    for _, d in ipairs(c.databases) do
      for _, s in ipairs(d.schemas) do
        local dir = manifest.path(c, d, s)
        uv.fs_chmod(dir, W)
        for _, t in ipairs(s.tables) do -- in two steps: `x` → `x.table` would replace `x.table`
          assert(os.rename(dir .. "/" .. t.file, dir .. "/" .. t.file .. ".tmp~"))
        end
        for _, t in ipairs(s.tables) do
          assert(os.rename(dir .. "/" .. t.file .. ".tmp~", dir .. "/" .. t.file .. "." .. t.kind))
          t.file = t.file .. "." .. t.kind
          old_files[#old_files + 1] = fs.rel(dir .. "/" .. t.file)
        end
        uv.fs_chmod(dir, tonumber("555", 8))
      end
    end
  end
  man.version = 1
  uv.fs_chmod(old_root, W)
  os.remove(old_root .. "/" .. fs.MANIFEST)
  local f = assert(io.open(old_root .. "/" .. fs.MANIFEST, "w"))
  f:write(vim.json.encode(man))
  f:close()
  uv.fs_chmod(old_root, tonumber("555", 8))
  manifest.reset()
  sync_.listed = {}
  local dir = "shop%~2/other.db/main/"
  eq(true, exists(dir .. "x.table") and exists(dir .. "x.table.table") and exists(dir .. "v.view"), "the old layout")
  eq(true, exists("Shop/shop.db/main/named.view"), "a view")
  -- An old name opened directly (harpoon, oldfiles) before anything is listed: its header says
  -- which table it is, so it's `x`, not a table named `x.table`.
  calls = {}
  local st = rows_buffer(dir .. "x.table")
  eq("x", st.spec.table, "the old file's table")
  eq("a", st.columns[1].name, "its column")
  local rows = vim.api.nvim_get_current_buf()
  eq("644", mode(dir .. "x.table"), "writable while loaded")
  vim.cmd("enew")
  -- The next listing (entering the root) renames everything in place, without connecting.
  calls = {}
  messages = {}
  enter("")
  eq({ "connections.list" }, methods(), "offline: only the root's own listing")
  eq({}, messages, "nothing to say")
  local on_disk = vim.json.decode(fs.read(old_root .. "/" .. fs.MANIFEST))
  eq(2, on_disk.version, "manifest v2 on disk")
  eq(nil, on_disk.migrate_from, "no marker saved")
  eq({ "big", "named", "orders", "users" }, ls("Shop/shop.db/main"), "Shop")
  for _, rel in ipairs(old_files) do
    if not vim.endswith(rel, "/x.table") then -- now the table named so
      eq(false, exists(rel), "old name gone: " .. rel)
    end
  end
  eq("x", sync_.parse_header(read(dir .. "x"):match("^[^\n]*")).table, "x")
  eq("x.table", sync_.parse_header(read(dir .. "x.table"):match("^[^\n]*")).table, "x.table")
  eq("view", sync_.parse_header(read(dir .. "v"):match("^[^\n]*")).kind, "v")
  assert(read("Shop/shop.db/main/users"):find("email", 1, true), "content kept")
  assert(vim.tbl_contains(ls("Staging/app_test/public"), "users"), vim.inspect(ls("Staging/app_test/public")))
  eq({}, ls("Staging/app"), "never-listed databases untouched")
  eq(nil, manifest.database(manifest.connection("staging-pg"), "app").listed, "and still not listed")
  -- The manifest and the disk agree, read-only.
  manifest.reset()
  local count = 0
  for _, c in ipairs(manifest.get().connections) do
    for _, d in ipairs(c.databases) do
      for _, s in ipairs(d.schemas) do
        local sdir = manifest.path(c, d, s)
        local listed_files = {}
        for _, t in ipairs(s.tables) do
          listed_files[t.file] = true
          count = count + 1
          local rel = fs.rel(sdir .. "/" .. t.file)
          eq(t.name, sync_.parse_header(read(rel):match("^[^\n]*")).table, "header of " .. rel)
          eq(t.name, manifest.lookup(rel).table.name, "lookup " .. rel)
          if rel ~= dir .. "x" then
            eq("444", mode(rel), "read-only " .. rel)
          end
        end
        for name in fs.list(sdir) do
          assert(listed_files[name], "on disk but not in the manifest: " .. sdir .. "/" .. name)
        end
        eq("555", mode(fs.rel(sdir)), "folder read-only")
      end
    end
  end
  eq(#old_files, count, "every table kept")
  -- The open buffer followed its file.
  eq(old_root .. "/" .. dir .. "x", bufname(rows), "renamed with its file")
  eq("644", mode(dir .. "x"), "still writable while loaded")
  vim.api.nvim_set_current_buf(rows)
  eq("x", state().spec.table, "the same rows")
  keys("-")
  eq(old_root .. "/shop%~2/other.db/main", bufname(), "- to its folder")
  vim.cmd("bwipeout! " .. rows)
  eq("444", mode(dir .. "x"), "read-only once closed")
  -- A second listing changes nothing; a sync of the databases agrees with the migration.
  local before = ls(dir)
  eq(nil, sync(""), "sync")
  eq(before, ls(dir), "same files")
  run("other", [[drop table "x"; drop table "x.table"; drop view "v"]])
  dbear.config.tree.root = mirror
  tree.attach()
  manifest.reset()
  sync_.listed = {}
  eq(mirror, fs.root(), "back")
end)

test("a v1 manifest whose files are gone or damaged: those databases are listed again", function()
  local sync_ = require("dbear.tree.sync")
  local root = mirror .. "-v1b"
  dbear.config.tree.root = root
  tree.attach()
  sync_.listed = {}
  eq(nil, sync("shop"), "Shop")
  local man = manifest.get()
  local W, R = tonumber("755", 8), tonumber("555", 8)
  local c = manifest.connection("shop")
  local d = c.databases[1]
  local s = d.schemas[1]
  local sdir = manifest.path(c, d, s)
  uv.fs_chmod(sdir, W)
  for _, t in ipairs(s.tables) do
    assert(os.rename(sdir .. "/" .. t.file, sdir .. "/" .. t.file .. "." .. t.kind))
    t.file = t.file .. "." .. t.kind
  end
  os.remove(sdir .. "/orders.table") -- gone
  uv.fs_chmod(sdir .. "/users.table", W)
  local f = assert(io.open(sdir .. "/users.table", "w")) -- no header
  f:write("garbage\n")
  f:close()
  uv.fs_chmod(sdir, R)
  man.version = 1
  uv.fs_chmod(root, W)
  os.remove(root .. "/" .. fs.MANIFEST)
  f = assert(io.open(root .. "/" .. fs.MANIFEST, "w"))
  f:write(vim.json.encode(man))
  f:close()
  uv.fs_chmod(root, R)
  manifest.reset()
  sync_.listed = {}
  calls = {}
  enter("")
  eq({}, vim.tbl_filter(function(m) return m ~= "connections.list" end, methods()), "offline")
  eq({ "big", "named" }, ls("Shop/shop.db/main"), "what could be kept")
  eq(nil, manifest.connection("shop").listed, "to be listed again")
  calls = {}
  enter("Shop")
  assert(vim.tbl_contains(methods(), "schemas.list shop"), vim.inspect(methods()))
  eq({ "big", "named", "orders", "users" }, ls("Shop/shop.db/main"), "listed again")
  dbear.config.tree.root = mirror
  tree.attach()
  manifest.reset()
  sync_.listed = {}
end)

print(string.format("\n%d passed, %d failed", passed, failures))
dbear.rpc():stop()
os.exit(failures == 0 and 0 or 1)
