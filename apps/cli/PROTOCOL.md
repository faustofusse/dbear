# `dbear-cli serve` protocol

`dbear-cli serve` speaks [JSON-RPC 2.0](https://www.jsonrpc.org/specification). Each message is one
line of JSON ending in `\n` (newline-delimited JSON, no `Content-Length` headers). The server
never writes anything else to stdout. Logs and warnings go to stderr.

Protocol version: **1** (`server.info`'s `protocol`). New methods and new result fields don't
change it, so ignore fields you don't know.

## Transports

| | |
|---|---|
| `dbear-cli serve` | stdio, one client. At the end of stdin, requests and jobs still in flight finish and answer, then the server exits. |
| `dbear-cli serve --socket [PATH]` | A Unix domain socket for many clients (Unix only). The default path is `serve.sock` beside the connection store (`~/Library/Application Support/dbear/serve.sock` on macOS, `$XDG_CONFIG_HOME/dbear/serve.sock` on Linux). `$DBEAR_SOCKET` overrides it. The socket is mode 0600. A leftover socket file that nobody answers on is replaced. If a server already answers there, the new one refuses to start (exit 1). The file is removed on exit (SIGINT, SIGTERM or `server.shutdown`). When a socket client hangs up, its running queries and jobs are cancelled. |

The one-shot commands (`dbear-cli query …`) use this same protocol. They connect to the socket
when a server is listening there and serves the same connection store, and otherwise run an
in-process session. `--no-server` forces the in-process session.

Environment (read by both the server and the one-shot commands):

| Variable | Effect |
|---|---|
| `DBEAR_CONNECTIONS_FILE` | Use another connection store (`state.db`, which holds the history, sits beside it). |
| `DBEAR_SOCKET` | The socket path. |
| `DBEAR_NO_KEYRING=1` | Never read the OS keychain. Saved passwords aren't used. Meant for tests and sandboxes. |

## What the server shares and what it doesn't

Shared across all clients: the saved connections (read-only), the keychain, query history, and
Cloudflare OAuth grants. Also shared: one cached connection per target, used for metadata
(`schemas.list`, `columns.list`, `table.describe`, `table.rows`, `table.editable`, `databases.list`, `complete`),
with its schema and completion caches. Row edits (`table.changes.apply`) run there too, in a
transaction of their own on the driver's edit session.

Per client: the connections that `query.run` uses. Each client gets one session per target, so
temp tables, `SET`, open transactions and `query.cancel` never cross clients. These sessions close
when the client goes away or calls `disconnect`.

The store is re-read at most every 2 seconds, and immediately when a name isn't found, so
connections the apps add or edit show up. A connection that was edited gets new sessions.

The server never writes the connection store and never creates or deletes keychain entries. The
only exception is the token rotation of "Sign in with Cloudflare" connections. Passwords are never
returned, except by `connections.url` with `with_password: true`.

## Messages

```jsonc
// request (client → server)
{"jsonrpc":"2.0","id":1,"method":"query.run","params":{"connection":"app_dev","sql":"select 1"}}
// response
{"jsonrpc":"2.0","id":1,"result":{…}}
{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Table not found: ghost","data":{"kind":"table-not-found","message":"ghost"}}}
// notification (server → client: job progress); a client notification (no id) runs but gets no answer
{"jsonrpc":"2.0","method":"dump.progress","params":{…}}
```

- **Ids** can be numbers or strings. `query.cancel` refers to a request by its id.
- **Concurrency**: requests run concurrently, including requests from the same client.
  Responses come back in the order the requests *finish*, so match them by id.
- **Batches** (JSON arrays) are not supported.
- **`params`** may be omitted or `null` for methods without required params.
- **Unknown params** are ignored.

### Errors

| code | when | `data` |
|---|---|---|
| -32700 | The line isn't JSON (`id` is `null`). | none |
| -32600 | The JSON isn't a request (for example, it has no `method`). | none |
| -32601 | Unknown method. | none |
| -32602 | Bad params (missing or wrongly typed fields). | `{"kind":"invalid-config","message":"Invalid params: …"}` |
| -32603 | Internal error. | none |
| -32000 | A dbear error. | the `dbcore::Error`: `{"kind": K, "message": M}` |

`message` is the text to show the user. `data.kind` is one of `connection-failed`,
`table-not-found`, `unsupported`, `query`, `cancelled` (no `message`), `invalid-config` (unknown or
ambiguous connection, bad URL…), `storage`, `internal`.

A **failing SQL statement is not a JSON-RPC error**. `query.run` succeeds and its outcome holds
`{"error": …}` (see below). JSON-RPC errors from `query.run` mean the statement never ran: unknown
connection, bad params, or the keychain or connection setup failed.

## Common shapes

These are dbcore's serde models.

**Connection spec (`connection`)**: a saved connection's id, or its exact name (trimmed and
case-insensitive; an ambiguous name is an `invalid-config` error that lists the ids), or a URL
(`postgres://…`, `mysql://…`, `sqlserver://…`, `libsql://…`, `d1://…`, `sqlite:/path`,
`file:/path`). A URL isn't saved. It gets an id `url:<hex>` that resolves for the life of the
server, and its password comes from the URL.

**`database`** (optional): another database on the connection's server. When it's absent, the
server uses the database last browsed in the apps (for connections that list all databases), or
else the connection's own database.

**Target**: `{"connection_id": "8a3f", "database": "app_dev"}`. `database` is always a name: the
connection's own database when it isn't switched (`"postgres"` for Postgres with none, `""` for
MySQL with none, the file path for SQLite). Results that resolve a connection return the target
they used.

**Value** (a cell):

| JSON | Meaning |
|---|---|
| `null` | NULL |
| `true` / `false` | Booleans |
| `"text"` | Text. Binary data is a hex preview such as `"0x0102"`. |
| `42` | An integer up to ±(2^53−1). |
| `{"int":"9007199254740993"}` | A larger integer, as a string. |
| `{"float":1.5}` | A float. NaN and infinities are `{"float":"NaN"}`, `{"float":"Infinity"}` and `{"float":"-Infinity"}`. |
| `{"decimal":"12.30"}` | An exact numeric, as text. |

A bare number is always an integer. Text that looks like a number (`"42"`) is still text.

**QueryResult**:

```json
{"columns":[{"name":"id","type_name":"int8","is_primary_key":false,"is_nullable":true}],
 "rows":[[1],[2]], "total_count":null, "truncated":false, "rows_affected":null, "origins":[]}
```

- `truncated`: more rows than the cap, which keeps the first rows. `total_count` is then the number
  the statement returned, when known.
- `rows_affected`: set for statements that return no rows (no `columns`).
- `origins[i]` is `{"schema","table","column"}` or `null`.

**ConnectionConfig** (no secrets):

```json
{"id":"8a3f","name":"app","group":"Work","kind":"postgres","host":"db.internal","port":5432,
 "database":"app_dev","user":"app","ssl_mode":"verify-full","show_all_databases":true,
 "ssh":{"host":"bastion","port":null,"user":"me","auth":"private-key","key_path":"~/.ssh/id_ed25519"}}
```

- `kind`: `postgres`, `mysql`, `sqlite`, `libsql`, `sqlserver` or `d1`.
- `ssl_mode`: `disable`, `prefer`, `require` or `verify-full`.
- `ssh.auth`: `password`, `private-key` or `agent`.

## Methods

### `server.info`
- Params: none.
- Result: `{"name":"dbear-cli","version":"0.1.0","protocol":1,"pid":123,"transport":"stdio"|"socket"|"in-process","store_path":"…/dbear.db","problems":[{"part":"secrets"|"state","error":Error}]}`
- `problems` lists what didn't open; the server still works without it. For example, with no keychain, saved passwords aren't available.

### `server.shutdown`
- Params: none.
- Result: `{}`, sent before stopping.
- On stdio the server exits. On a socket, the whole server stops: every client's work is cancelled and the socket file is removed.

### `connections.list`
- Params: none.
- Result: `{"connections":[Listed…]}`, in the user's order.
- `Listed` is a ConnectionConfig plus `"summary"` (for example `"PostgreSQL · localhost:5432/app_dev"`) and `"browses_databases"` (the connection offers its server's other databases).

### `connections.get`
- Params: `{"connection"}`.
- Result: a `Listed` plus `"target"`, the target it opens without a `database` param.

### `connections.url`
- Params: `{"connection", "with_password"?: false}`.
- Result: `{"url":"postgres://app@db.internal:5432/app_dev?sslmode=verify-full"}`.
- With `with_password`, the URL includes the keychain password.

### `databases.list`
- Params: `{"connection"}`.
- Result: `{"databases":["app_dev","postgres"],"default":"app_dev","current":"app_dev"}`.
- `databases` is the server's databases (Postgres, MySQL, SQL Server, D1). Other kinds list only their own.
- `default` is the connection's own database. `current` is where it opens without `database`.

### `schemas.list`
- Params: `{"connection","database"?,"refresh"?: false}`.
- Result: `{"target":Target,"schemas":[{"name":"public","tables":[{"schema":"public","name":"users","kind":"table"|"view","estimated_row_count":248|null}]}]}`.
- The result is cached per target until DDL runs through `query.run`, a restore finishes, `refresh: true` is passed, or `disconnect` is called.

### `columns.list`
- Params: `{"connection","database"?}`.
- Result: `{"target":Target,"tables":[{"schema":"public","table":"users","columns":[{"name":"id","type_name":"int8","is_primary_key":true,"is_nullable":false}]}]}`.
- Every table's and view's columns in the target, in one call (dbcore's `Driver::list_columns`, what completion uses). Use it instead of a `table.describe` per table when you only need names and types. Not cached: each call asks the database.

### `table.describe`
- Params: `{"connection","database"?,"schema"?,"table"}`.
- Result: `{"target","table":TableInfo,"structure":TableStructure}`.
  - `TableStructure`: `{"columns":[{"name","type_name","is_nullable","default_value","is_primary_key","comment"}],"primary_key":[…],"indexes":[{"name","columns","is_unique","is_primary","definition"}],"foreign_keys":[{"name","columns","referenced_schema","referenced_table","referenced_columns","on_update","on_delete"}],"referenced_by":[{"schema","table","name","columns","referenced_columns"}],"ddl":"CREATE TABLE …"|null}`.
- Finding the table when `schema` is absent:
  - It's looked up by name, exactly first, then case-insensitively.
  - If several schemas have it, `public`/`main`/`dbo` wins; otherwise it's an `invalid-config` error that lists them.
  - `"a.b"` also matches schema `a`, table `b`.
- Unknown table: `table-not-found`.

### `table.rows`
- Params: `{"connection","database"?,"schema"?,"table","query"?:{"sort":[{"column","descending"}],"filter":"status = 'paid'"},"limit"?:100,"cursor"?}`.
- Result: `{"target","table":TableInfo,"result":QueryResult,"next":"<cursor>"|null}`.
- Pages are stable: rows are sorted by your columns, then the primary key or row id.
- To get the next page, pass `next` back as `cursor` together with the same `query`. Treat it as opaque.
- `filter` is a single SQL boolean expression; a `;` between statements is rejected.
- This method runs on the shared metadata session.

### `table.editable`
- Params: `{"connection","database"?,"schema"?,"table"}` (found like `table.describe`).
- Result: `{"target","table":TableInfo,"read_only":"“named” is a view."|null,"key":["id"],"columns":[EditableColumn…],"foreign_keys":[…],"referenced_by":[…]}`.
  - `read_only`: why no row can be changed (a view, no primary key); `null` when they can. `key` is the primary key, in key order (empty when read-only).
  - `EditableColumn`: `{"name","type_name","is_primary_key","is_nullable","default_value","binary","read_only"}`. `binary` columns come as a hex preview (`"0x…"`, cut), so only `null`/`default` can be written to them; `read_only` says why a column can't be changed.
  - `foreign_keys`: `{"columns":["user_id"],"schema","table","target_columns":["id"],"label"}`: a row's `columns` point at `target_columns` of `schema.table` (empty: its primary key, an implicit SQLite reference).
  - `referenced_by`: `{"values":["id"],"schema","table","columns":["user_id"],"label"}`: rows of `schema.table` whose `columns` hold this row's `values` columns.
- The rules are the apps' table grids' (`dbcore::results::ResultSources`). Names, not indexes: match them with `table.rows`' `columns`.

### `table.changes.preview`
- Params: `{"connection","database"?,"schema"?,"table","changes":[RowChange…]}`.
  - `RowChange` (dbcore's `edit::RowChange`):
    - `{"kind":"update","key":[KeyValue…],"set":[CellEdit…]}`
    - `{"kind":"delete","key":[KeyValue…]}`
    - `{"kind":"insert","values":[CellEdit…]}`
  - `KeyValue`: `{"column","value":Value}`, every primary key column with the row's value **as loaded** (send back what `table.rows` returned).
  - `CellEdit`: `{"column","value":EditValue}`; `EditValue` is `"null"`, `"default"` (UPDATE: `DEFAULT`, not on SQLite; INSERT: the column is left out) or `{"text":"…"}`. Text is sent as a string literal and cast by the database (`'42'` into an integer, `'{a,b}'` into an array).
- Result: `{"target","table","statements":[{"sql","expect_one_row","target"}]}`, in the order they'd run: deletes, updates, inserts. Nothing runs.
- Refused (`unsupported` or `query` errors) before any SQL: any change to a read-only table, text into a binary or read-only column, an unknown column, a key that isn't the whole primary key.

### `table.changes.apply`
- Params: those of `table.changes.preview`, plus `"reviewed"?:["DELETE …", …]`: the `sql` of the statements the user confirmed. When given, the server refuses (nothing runs) unless it would run exactly those, e.g. if the table changed since the preview.
- Result: `{"target","table","rows_affected":3}`.
- Runs in one transaction on the target's shared connection (the driver's edit session): every UPDATE and DELETE must match exactly one row, and a failing statement or check rolls everything back. The error then says which row and that nothing was saved.

### `table.related`
- Params: `{"connection","database"?,"schema"?,"table","columns"?:["user_id"],"values":[Value…]}`.
- Result: `{"target","table","filter":"\"user_id\" = 1"}`, a `filter` for `table.rows` that selects the rows of `table` whose `columns` hold `values` (`is null` for nulls). Without `columns`, the table's primary key.
- Follow a foreign key with its `table`/`target_columns` and the row's values in `columns`; list the rows referencing a row with a `referenced_by`'s `table`/`columns` and the values in its `values`.

### `query.run`
- Params: `{"connection","database"?,"sql","max_rows"?,"record_history"?: false,"describe_sources"?: false}`.
  - `sql` can hold several statements; the result is the last statement's.
  - `max_rows`: absent means 10 000. `null` or `0` means all rows.
  - `record_history` adds the run to the connection's history, shared with the apps, which keep the newest 1000 entries. Cancelled runs are never recorded.
  - `describe_sources` also returns `sources`: which table each column comes from and whether the rows are editable (`dbcore::results::ResultSources`).
- Result: `{"target":Target,"result":{"ok":QueryResult}|{"error":Error},"duration_ms":12,"schema_changed":false,"sources":…|null,"history_error"?:Error}`.
  - A cancelled run gives `"result":{"error":{"kind":"cancelled"}}`.
  - `schema_changed` means the SQL looked like DDL (and wasn't cancelled). The target's schema and completion caches were dropped, so reload any table list you show.
- Runs on this client's own session for the target. While it runs, the client's other requests are still answered.
- Two `query.run`s of one client on the **same target** share that session, so they queue, and cancelling one cancels whatever runs there.

### `query.cancel`
- Params: `{"id"?}` (the `query.run` request's id), or `{"connection","database"?}`, or `{}` (all of this client's running queries).
- Result: `{"cancelled": n}`, the number of running queries it found.
- The cancelled `query.run` then answers with the cancelled outcome. Only this client's queries can be cancelled.

### `complete`
- Params: `{"connection","database"?,"sql","offset"}`. `offset` is the cursor's **UTF-8 byte** offset in `sql`.
- Result: `{"target","replace_start":14,"replace_end":16,"items":[{"label":"users","insert_text":"users","kind":"table","detail":"public"}]}`.
  - Replace bytes `replace_start..replace_end` of `sql` with an item's `insert_text`.
  - `kind`: `keyword`, `schema`, `table`, `view`, `column` or `function`.
- Tables and columns are listed once per target and cached. If they can't be listed, only keywords are returned.

### `history.list`
- Params: `{"connection","limit"?: 50}`.
- Result: `{"entries":[{"id":3,"connection_id":"8a3f","database":"app_dev","sql":"select 1","ran_at":1700000000000,"duration_ms":12,"rows":1,"error":null}]}`, newest first. `ran_at` is milliseconds since the epoch.

### `dump.start`
- Params: `{"connection","database"?,"path"?,"content"?,"compression"?,"data_style"?,"drop_objects"?: false,"create_database"?: false,"scope"?}`.
  - `path`: absolute, or relative to the server's working directory. Absent means `<database>-<YYYY-MM-DD>.sql[.gz]` in that directory.
  - `content`: `schema-and-data` (default), `schema-only` or `data-only`.
  - `compression`: `none` or `gzip`. Absent means gzip when `path` ends in `.gz`.
  - `data_style`: `copy` (default) or `insert`. Only Postgres uses COPY; other databases always use INSERTs.
  - `create_database`: MySQL only.
  - `scope`: `"database"` (default), `{"schemas":["public"]}` or `{"tables":[{"schema":"public","name":"users","kind":"table","estimated_row_count":null}]}`.
- Result (right away): `{"job":"dump-1","target":Target,"path":"/abs/path.sql"}`.
- Then come notifications, always **after** this response:
  - `dump.progress`, about 10 per second: `{"job","progress":{"phase":"connecting"|"schema"|"data"|"post-data"|"finishing","object":"public.users"|null,"tables_done","tables_total","rows_done","table_rows_done","table_rows_estimate","bytes_written"},"fraction":0.4|null}`.
  - `dump.finished`, exactly once: `{"job","result":{"ok":{"tables","rows","bytes","warnings":[…]}}|{"error":Error}}`.
- The dump is written to `<path>.partial` and renamed when done. It reads one consistent snapshot over a session of its own.

### `restore.start`
- Params: `{"connection","database"?,"path","single_transaction"?,"stop_on_error"?}`.
  - The defaults are one transaction (except on MySQL, which commits DDL as it goes) and stopping at the first error.
  - A transaction always stops at the first error.
  - `.gz` files are read compressed.
- Result: `{"job":"restore-2","target","path"}`. A missing file is an `invalid-config` error.
- Notifications:
  - `restore.progress`: `{"job","progress":{"bytes_read","bytes_total","statements","errors"},"fraction"}`.
  - `restore.finished`: `{"job","result":{"ok":{"statements","rows","errors":["Line 7: …"],"error_count","warnings"}}|{"error":Error}}`.
- When it ends, the target's schema caches are dropped.

### `job.cancel`
- Params: `{"job"}`.
- Result: `{"cancelled": true}` when the job was still running.
- The job then finishes with `{"error":{"kind":"cancelled"}}`, and a cancelled dump leaves no file.

### `disconnect`
- Params: `{"connection"?}`.
- Result: `{"connections":["8a3f"]}`, the ids that had sessions open.
- Closes this client's sessions for that connection (or all of them), plus the shared metadata session and caches. Everything reconnects on next use.

## Example session (stdio)

```text
→ {"jsonrpc":"2.0","id":1,"method":"server.info"}
← {"jsonrpc":"2.0","id":1,"result":{"name":"dbear-cli","version":"0.1.0","protocol":1,"pid":4242,"transport":"stdio","store_path":"/Users/me/Library/Application Support/dbear/dbear.db","problems":[]}}
→ {"jsonrpc":"2.0","id":2,"method":"connections.list"}
← {"jsonrpc":"2.0","id":2,"result":{"connections":[{"id":"8a3f","name":"app_dev","group":"","kind":"postgres","host":"localhost","port":54329,"database":"app_dev","user":"postgres","ssl_mode":"prefer","show_all_databases":true,"ssh":null,"summary":"PostgreSQL · localhost:54329/app_dev","browses_databases":true}]}}
→ {"jsonrpc":"2.0","id":3,"method":"complete","params":{"connection":"app_dev","sql":"select * from us","offset":16}}
← {"jsonrpc":"2.0","id":3,"result":{"target":{"connection_id":"8a3f","database":"app_dev"},"replace_start":14,"replace_end":16,"items":[{"label":"users","insert_text":"users","kind":"table","detail":"public"}]}}
→ {"jsonrpc":"2.0","id":4,"method":"query.run","params":{"connection":"app_dev","sql":"select pg_sleep(30)"}}
→ {"jsonrpc":"2.0","id":5,"method":"query.run","params":{"connection":"app_dev","sql":"select id, name from users limit 1"}}
← {"jsonrpc":"2.0","id":5,"result":{"target":{"connection_id":"8a3f","database":"app_dev"},"result":{"ok":{"columns":[{"name":"id","type_name":"int8","is_primary_key":true,"is_nullable":false},{"name":"name","type_name":"text","is_primary_key":false,"is_nullable":false}],"rows":[[1,"Alan Turing"]],"total_count":null,"truncated":false,"rows_affected":null,"origins":[…]}},"duration_ms":3,"schema_changed":false,"sources":null}}
→ {"jsonrpc":"2.0","id":6,"method":"query.cancel","params":{"id":4}}
← {"jsonrpc":"2.0","id":6,"result":{"cancelled":1}}
← {"jsonrpc":"2.0","id":4,"result":{"target":{…},"result":{"error":{"kind":"cancelled"}},"duration_ms":1840,"schema_changed":false,"sources":null}}
→ {"jsonrpc":"2.0","id":7,"method":"dump.start","params":{"connection":"app_dev","path":"/tmp/app.sql.gz"}}
← {"jsonrpc":"2.0","id":7,"result":{"job":"dump-1","target":{…},"path":"/tmp/app.sql.gz"}}
← {"jsonrpc":"2.0","method":"dump.progress","params":{"job":"dump-1","progress":{"phase":"data","object":"public.users",…},"fraction":0.4}}
← {"jsonrpc":"2.0","method":"dump.finished","params":{"job":"dump-1","result":{"ok":{"tables":10,"rows":53806,"bytes":516556,"warnings":[]}}}}
→ {"jsonrpc":"2.0","id":8,"method":"table.describe","params":{"connection":"app_dev","table":"ghost"}}
← {"jsonrpc":"2.0","id":8,"error":{"code":-32000,"message":"Table not found: ghost","data":{"kind":"table-not-found","message":"ghost"}}}
```

(In the example the keys are in a readable order. The server writes them in alphabetical order;
key order carries no meaning.)
