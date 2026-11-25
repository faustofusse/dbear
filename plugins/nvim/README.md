# dbear.nvim

Run SQL from Neovim on the connections you saved in dbear. The plugin starts `dbear-cli serve`
and talks to it over stdio ([protocol](../../apps/cli/PROTOCOL.md)), so it uses the same
connections, keychain passwords and query history as the apps. It needs Neovim 0.10+ and
`dbear-cli`. It has no other dependencies.

## Install

Build the CLI (`cargo build -p dbear-cli`, or `cargo install --path apps/cli`), then add this folder
to your runtime path:

```lua
-- lazy.nvim
{ dir = "~/src/dbear/plugins/nvim", opts = { bin = "~/src/dbear/target/debug/dbear-cli" } }
-- or by hand
vim.opt.rtp:append("~/src/dbear/plugins/nvim")
require("dbear").setup({})
```

`setup` is optional. These are the defaults:

```lua
require("dbear").setup({
  cmd = { "dbear-cli", "serve" }, -- the server command; `bin = "/path/dbear-cli"` is a shortcut
  env = nil,                      -- extra environment, e.g. { DBEAR_CONNECTIONS_FILE = "…" }
  max_rows = 1000,                -- rows kept per result (0: all)
  record_history = true,          -- add runs to the connection's history, shared with the apps
  null = "NULL",                  -- NULL's text. It's highlighted (DbearNull), so it never looks like the text 'NULL'
  max_width = 60,                 -- cut wider cells with … (display cells; 0: never)
  split = "botright 15split",     -- how the results window opens
})
```

## Commands

| | |
|---|---|
| `:Dbear [name]` | Set the buffer's connection: a saved connection's name or id, or a URL. Without an argument, pick one from a list. |
| `:DbearDatabase [name]` | Use another database on the connection's server. Without an argument, pick one. |
| `:DbearRun` | Run the buffer. With a range, run those lines. From visual mode (`:'<,'>DbearRun`), run exactly the selection. |
| `:DbearCancel` | Stop the queries this buffer started. |
| `:DbearTables[!]` | List the tables and views. `!` reloads them. |
| `:DbearDescribe [table]` | Show a table's columns, indexes, foreign keys and DDL. The default is the word under the cursor. |
| `:DbearHistory` | Pick a query from the connection's history and insert it below the cursor. |

Results go to one reusable `dbear://results` split. The first line has the row count and the
duration, or the error. Cells are aligned by display width, with numbers on the right and control
characters escaped (`\n`).

Setting a connection also sets the buffer's `omnifunc`, so `<C-x><C-o>` completes keywords,
tables and columns from the server's `complete` method.

The connection is in `b:dbear_connection` (an id, name or URL) and `b:dbear_database`, so you can set
them from an ftplugin or a modeline. `g:dbear_connection` is the fallback for every buffer.
Highlight groups: `DbearHeader`, `DbearNull`, `DbearError` and `DbearSummary`.

## From Lua

`require("dbear.rpc")` is a JSON-RPC client with no UI code:

```lua
local client = require("dbear.rpc").new({ cmd = { "dbear-cli", "serve" } })
client:request("query.run", { connection = "app_dev", sql = "select 1" }, function(err, res) … end)
local err, res = client:request_sync("connections.list")
```

It starts the server on the first request and restarts it after it exits. Requests that are still
waiting when the server exits fail with code -32099. `require("dbear").rpc()` returns the plugin's
own client.

## Tests

`plugins/nvim/tests/run.sh [path/to/dbear-cli]` builds the CLI (when no path is given). It seeds a
temp connection store with a SQLite connection and runs `tests/spec.lua` in `nvim --headless -u NONE`.
