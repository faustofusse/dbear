if vim.g.loaded_dbear then
  return
end
vim.g.loaded_dbear = true

for group, link in pairs({ DbearHeader = "Title", DbearNull = "Comment", DbearError = "ErrorMsg", DbearSummary = "Comment" }) do
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
