if vim.g.loaded_dbear then
  return
end
vim.g.loaded_dbear = true

for group, link in pairs({
  DbearHeader = "Title",
  DbearNull = "Comment",
  DbearError = "ErrorMsg",
  DbearSummary = "Comment",
  DbearChanged = "DiffText",
  DbearCut = "NonText",
  DbearSeparator = "NonText",
  DbearReadOnly = "WarningMsg",
  DbearDelete = "DiffDelete",
  DbearUpdate = "DiffChange",
  DbearInsert = "DiffAdd",
}) do
  vim.api.nvim_set_hl(0, group, { link = link, default = true })
end

local function dbear()
  return require("dbear")
end

local command = vim.api.nvim_create_user_command

command("Dbear", function(o) dbear().connect(o.args) end, {
  nargs = "?",
  desc = "Pick or set the buffer's dbear connection",
  complete = function(lead)
    return vim.tbl_filter(function(n) return vim.startswith(n:lower(), lead:lower()) end, dbear().connection_names())
  end,
})
command("DbearDatabase", function(o) dbear().database(o.args) end, { nargs = "?", desc = "Pick the buffer's database" })
command("DbearRun", function(o) dbear().run(o) end, { range = true, desc = "Run the buffer, range or selection" })
command("DbearTables", function(o) dbear().tables(o.bang) end, { bang = true, desc = "List tables (! reloads)" })
command("DbearDescribe", function(o) dbear().describe(o.args) end, { nargs = "?", desc = "Describe a table (default: word under the cursor)" })
command("DbearCancel", function() dbear().cancel() end, { desc = "Cancel this buffer's running queries" })
command("DbearHistory", function() dbear().history() end, { desc = "Insert a query from the connection's history" })

-- The browser: `dbear://` buffers (see :h dbear / README).
vim.api.nvim_create_autocmd("BufReadCmd", {
  group = vim.api.nvim_create_augroup("dbear.browse", { clear = true }),
  pattern = "dbear://*",
  callback = function(ev) require("dbear.browse").read(ev.buf, vim.api.nvim_buf_get_name(ev.buf)) end,
})

local function browse()
  return require("dbear.browse")
end

command("DbearOpen", function(o) browse().open(o.args, o.bang) end, {
  nargs = "?",
  bang = true,
  desc = "Browse: the mirror's folders (! the dbear:// list), or conn[/db]/[schema.table]",
  complete = function(lead)
    return vim.tbl_filter(function(n) return vim.startswith(n:lower(), lead:lower()) end, dbear().connection_names())
  end,
})
command("DbearSort", function(o) browse().sort(o.args) end, {
  nargs = "?",
  desc = "Sort a dbear table buffer: column [desc], … (none: natural order)",
  complete = function() return browse().columns() end,
})
command("DbearFilter", function(o) browse().filter(o.args) end, { nargs = "?", desc = "Filter a dbear table buffer with a SQL WHERE expression" })

-- The mirror (lua/dbear/tree): folders and table files under `tree.root`. Registering the
-- autocommands touches nothing on disk and starts nothing.
local function tree()
  return require("dbear.tree")
end
tree().attach()

command("DbearSync", function(o) tree().sync(o.args, o.bang) end, {
  nargs = "?",
  bang = true,
  desc = "List the mirror again (a connection: all its databases); ! rebuilds",
  complete = function(lead)
    return vim.tbl_filter(function(n) return vim.startswith(n:lower(), lead:lower()) end, dbear().connection_names())
  end,
})
command("DbearRename", function(o) tree().rename(o.args) end, {
  nargs = "?",
  desc = "Rename the current connection (mirror folder, table or SQL buffer) in dbear's store",
})
command("DbearFind", function() tree().find() end, { desc = "Find a table in the mirror (telescope when installed)" })
command("DbearGrep", function(o) tree().grep(o.args) end, { nargs = "?", desc = "Search the mirror's column lists" })
