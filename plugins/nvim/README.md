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
  browse = {
    page_size = 100,              -- rows per page in table buffers
    max_width = 40,               -- wider values are cut (…) in table buffers; edit them with <CR>
  },
  tree = {                        -- the mirror (below); `tree = { enabled = false }` turns it off
    enabled = true,
    root = nil,                   -- default: stdpath("cache") .. "/dbear/tree"
    relist = "session",           -- list a folder again the first time it's entered in a session; "never": only once
  },
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
| `:DbearOpen[!] [path]` | Browse. Without `!` and with the mirror on (the default): no `path` opens the [mirror](#the-mirror-oilnvim-telescope-and-other-file-tools)'s root, a connection's name or id its folder. Otherwise the `dbear://` browser (below): `path` is `conn`, `conn/db/`, `conn/schema.table`, a `dbear://` name or a connection URL; without one, the buffer's connection, else the list of connections. |
| `:DbearSync[!] [conn]` | List the mirror again: without `conn`, the connection folders and everything listed before; with `conn`, all its databases and tables. `!` rebuilds what it lists. |
| `:DbearFind` | Pick a table of the mirror: telescope's `find_files` when it's installed, else `vim.ui.select`. |
| `:DbearGrep [pattern]` | Search the mirror's column lists: telescope's `live_grep` when it's installed, else ripgrep (or grep) into the quickfix list. |
| `:DbearSort [col [desc], …]` | In a table buffer: sort (no argument: the table's natural order). |
| `:DbearFilter [expr]` | In a table buffer: a raw SQL `WHERE` expression (no argument: none). |

Results go to one reusable `dbear://results` split. The first line has the row count and the
duration, or the error. Cells are aligned by display width, with numbers on the right and control
characters escaped (`\n`).

## Browsing and editing tables

Like [oil.nvim](https://github.com/stevearc/oil.nvim), the browser is made of ordinary buffers named
`dbear://…`. You move with `<CR>` (enter) and `-` (up) in the current window, and `:e dbear://…`
opens any of them:

| Buffer | Shows |
|---|---|
| `dbear://` | The saved connections. |
| `dbear://<conn>/` | The connection's databases (when it browses several), else its tables. |
| `dbear://<conn>/<db>/` | The database's tables and views, by schema, with their kind and estimated rows. |
| `dbear://<conn>/[<db>/]<schema>.<table>` | The table's rows, editable. `?where=…&sort=a,-b` filters and sorts. |

`<conn>` is a connection's name or id (or a URL, percent-encoded; `:DbearOpen` does that for you).
The lists are read-only.

A table buffer has one line per row and the column names in the window bar. Long values are cut
(`…`), lines don't wrap (`zl`/`zh` scroll, the header follows). The footer says how many rows are
loaded, the sort, the filter, and what's pending. Views and tables without a primary key open
read-only, with the reason in the footer.

Keys in a table buffer (`g?` lists them):

| | |
|---|---|
| `<CR>` | Edit the cell's whole value in a buffer of its own. `:w` there puts it in the row. |
| `-` | Up to the table list. |
| `gf` | Open the row the foreign key under the cursor points at (a filtered table buffer). |
| `gR` | Open the rows of other tables that reference this row. |
| `gs` | Sort by the column under the cursor: ascending, descending, off. |
| `g/` | Filter with a SQL `WHERE` expression. |
| `]]` | Load the next page. It also loads by itself near the end when no change is pending. |
| `<C-l>` | Reload (asks before discarding pending changes). `:e!` discards them without asking. |
| `gd` | The table's structure, in the results split. |
| `o` / `O` | A new row below/above, in Insert mode in its first non-key cell. |

### Editing rows

Edit the text with any Vim command, then `:w`:

- **Change a cell**: an UPDATE of that cell (only the cells whose text changed are written).
- **Delete a line** (`dd`, `:g/…/d`…): a DELETE.
- **A new line**, or **a copy of a line** (`yyp`): an INSERT. Cells left as they were copied keep
  the copied row's whole value (even when cut); an empty cell is the column's DEFAULT.
- **Moving lines** changes nothing.

`:w` shows the SQL the server will run in a float: `y` or `<CR>` runs it in one transaction, `n`,
`q` or `<Esc>` cancels. Each UPDATE and DELETE must match exactly one row, and if anything fails
nothing is saved: the edits stay in the buffer and the error shows under the rows. After a save the
table reloads. The buffer is `modified` only while changes are pending (undoing every edit makes it
clean again). Lines that can't be read (a wrong number of cells, a bad escape, a damaged row id) are
listed with their line numbers, and nothing is saved until they're fixed.

Each line starts with a hidden row id (`/007:`, concealed) that ties it to the row as loaded, by its
primary key, so reordering, yanking, pasting, undo and redo all work. Don't type before it (`I` and
`0` keep the cursor after it).

Cells are separated by `|` and their text round-trips exactly:

| In a cell | Means |
|---|---|
| `\N` (the whole cell) | NULL |
| `\D` (the whole cell) | DEFAULT (in an UPDATE: not on SQLite) |
| `\e` (the whole cell) | the empty string |
| empty | the empty string in a loaded row, DEFAULT in a new row |
| `\\` | a backslash (and a pipe in a value is a backslash and a pipe) |
| `\n`, `\r`, `\t`, `\xHH` | newline, carriage return, tab, another control character |
| `\s` | a space (spaces at the start or end of a cell are otherwise padding) |

So the text `NULL` is just `NULL`, a value `a|b` is `a\|b`, and `C:\temp` is shown and typed as `C:\\temp`. Values go to the
database as text and are cast there (`42`, `{a,b}`, `2024-01-02 03:04:05+00`, JSON…).

Some cells can't be edited in the line, and `:w` refuses (with the line number) if they were:
values cut for display (`…`: use `<CR>`, which edits the whole value, newlines included), and binary
values (shown as a hex preview; only `\N` or `\D` can replace them). Lines are realigned when you
leave Insert mode (or after a Normal mode change); the realignment is part of the same undo step.

Rows load a page at a time with the server's keyset cursors. Loading a page while changes are
pending keeps them, but the undo history starts again there (undoing the load would delete rows).
Sorting and filtering reload the table, asking before discarding pending changes.

Limits: one table per buffer (no joins); edits need a primary key in the rows; binary values can't
be written; no DDL from the buffer (no renaming or dropping tables); a connection given as a URL keeps
its password in the buffer name.

Setting a connection also sets the buffer's `omnifunc`, so `<C-x><C-o>` completes keywords,
tables and columns from the server's `complete` method.

The connection is in `b:dbear_connection` (an id, name or URL) and `b:dbear_database`, so you can set
them from an ftplugin or a modeline. `g:dbear_connection` is the fallback for every buffer.
Highlight groups: `DbearHeader`, `DbearNull`, `DbearError` and `DbearSummary`; in table buffers also
`DbearChanged` (an edited cell), `DbearCut` (the `…`), `DbearSeparator`, `DbearReadOnly`, and
`DbearDelete`/`DbearUpdate`/`DbearInsert` (the confirmation and the signs).

## The mirror: oil.nvim, telescope and other file tools

The mirror is a folder of real directories and files, one per connection, database, schema and
table, so anything that works on paths can browse your databases: oil.nvim or netrw, telescope's
`find_files` and `live_grep`, `:e`, `gf`, fzf… It lives in `tree.root`
(`~/.cache/nvim/dbear/tree` by default):

```
<root>/<connection>/<database>/<schema>/<table>.table     (views: <view>.view)
Shop/shop.db/main/users.table                 SQLite: the file's name, then its schema "main"
Staging/app/public/users.table                Postgres: each database of the server, then its schemas
Work/app/app/users.table                      MySQL: the database, then the database again as its schema
```

The depth is always four, so a path reads the same for every kind of database: the database
folders are what the connection's server lists (connections that browse their server's databases:
Postgres, MySQL, SQL Server, D1), or else the connection's one database (named after the file for
SQLite, `default` when it has no name, e.g. libSQL or a MySQL connection without a database), and
the schema folders are what the database lists (`main` for SQLite, `public`, `dbo`…; MySQL has no
level below databases, so there the schema folder repeats the database's name).

**Opening a table file shows its rows** in the editable table buffer described above (same keys,
`:w` with the SQL confirmation, paging, `gs`, `g/`, `gf`/`gR`). The file itself is never changed by
editing rows. `-` goes to the table's folder (in oil if you use it); filtered views (`gf`, `gR`) are
`dbear://` buffers, and `-` there also goes back to the mirror's folder. The file holds a header
line with the table's exact target and its **columns** (name, type, `PK`, `NULL`/`NOT NULL`), so
`live_grep` on the root finds a column in every table listed so far, and telescope's previewer shows
the columns.

**Lazy.** Opening Neovim lists nothing and connects to nothing. Entering the root (in oil, netrw or
any directory buffer) reads the connections from the store (no network). Entering a connection's
folder lists its databases (or, for a connection with one database, its tables); entering a
database or schema folder lists its tables. Each folder is listed again the first time you enter it
in a session (`relist = "never"`: only once). While it's listed, the folder's buffer says so under
its first line (`dbear: listing Staging…`), and it's redrawn when done (oil's `watch_for_changes`
isn't needed). A failed listing (server down, keychain denied) leaves the previous tree as it was;
the error is shown there (`dbear: ✗ …`) and as a message, and `:DbearSync <conn>` tries again.
`:DbearSync` (no argument) only lists what was listed before, so it never reaches a server you
haven't browsed. Dropped tables and removed connections disappear on the next listing; a renamed
connection's folder is renamed with its contents.

**Read-only.** The mirror's folders are read-only (mode 555) and its files too (444), so oil (even
with `skip_confirm_for_simple_edits`) shows them as not modifiable, and a forced delete or rename
fails with "permission denied" instead of looking like a dropped table. A table file is writable
while its rows buffer is loaded, because Vim refuses `:w` on a file it can't write even when `:w`
saves rows; its folder stays read-only. Only the plugin lifts and restores the permissions, and
every change it makes on disk is checked against the root: nothing outside it is ever written,
chmod-ed, renamed or deleted, symlinks are never followed (one found in the mirror is unlinked),
and a root that isn't empty and holds no `.dbear-mirror.json` is refused.

**Names.** A file name is the database name, escaped where it has to be: `%` → `%25`;
`/ \ : * ? " < > |`, control characters and bytes that aren't UTF-8 → `%XX`; a leading or trailing
`.` or space → `%XX`; an empty name → `%-`. Two names that the file system can't tell apart (`Users`
and `users` on macOS, or two Unicode spellings of `é`) are numbered: the second one gets `%~2`.
Names longer than 200 bytes are cut and end with `%+<hash>`. Connection folders use the
connection's name (two connections called `Shop` and `shop` give `Shop` and `shop%~2`). Nothing is
read back from a file name: `.dbear-mirror.json` (at the root) maps every folder to its exact
connection id, database and schema, and each table file's first line
(`#dbear-mirror v1 {"connection":…,"database":…,"schema":…,"table":…}`) to its table. Connections
given as URLs are never mirrored, and no password or token is written there.

Suggested setup (oil and telescope are optional):

```lua
vim.keymap.set("n", "<leader>do", "<cmd>DbearOpen<cr>", { desc = "dbear: browse (mirror root)" })
vim.keymap.set("n", "<leader>df", "<cmd>DbearFind<cr>", { desc = "dbear: find a table" })
vim.keymap.set("n", "<leader>dg", "<cmd>DbearGrep<cr>", { desc = "dbear: find a column" })
vim.keymap.set("n", "<leader>ds", "<cmd>DbearSync<cr>", { desc = "dbear: sync the mirror" })
```

From Lua: `require("dbear.tree").find()`, `.grep(pattern)`, `.sync(conn, rebuild)`, `.root()`.

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
temp connection store with a SQLite connection and runs `tests/spec.lua` in `nvim --headless -u NONE`,
then seeds another one ("Shop") and runs `tests/browse_spec.lua`, which edits tables with Vim
commands and checks the database after every save, then a third one (SQLite files plus the mock
Postgres connections) for `tests/tree_spec.lua`, the mirror, in a temp root. When oil.nvim,
telescope.nvim and plenary.nvim are in `$DBEAR_TEST_PLUGINS` (default
`~/.local/share/nvim/site/pack/core/opt`, used read-only), `tests/tree_plugins_spec.lua` drives the
mirror through them too.
