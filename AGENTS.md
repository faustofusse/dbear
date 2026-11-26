# AGENTS.md

- Database logic (drivers, decoding, SQL, connection storage, URL parsing) belongs in `crates/dbcore`. It has no FFI or UI deps.
- The glue around it that every Rust client needs (stores + keychain → `Connection`, targets, caches, scripts with
  history, connection CRUD orderings, dump/restore options) belongs in `crates/dbservice`. GPUI and `dbear-cli` use
  it; the macOS app and `dbcore-ffi` don't (yet).
- `dbear-cli` writes the connection store in exactly one way: `connections.rename` (`Service::rename` →
  `ConnectionStore::rename`, a targeted `update … set name` of one saved connection; nothing else in the row, no
  adds, edits or deletes). It never writes the keychain (only OAuth token rotation). Plugins talk to
  `dbear-cli serve` and nothing else; `apps/cli/PROTOCOL.md` is their contract: keep it in step with `wire.rs`.
- `crates/dbcore-ffi` is the only FFI surface. Only `apps/macos/Sources/DBKit/RustDriver.swift` imports `DBCoreFFI`.
- Rows cross the FFI boundary in pages (`QueryResult`), never one cell at a time.
- Passwords never touch disk. `dbcore::secrets` keeps them in the OS store (feature `os-keyring`: Keychain, Secret Service, Credential Manager), using service `ar.fausto.dbear.connection` and the connection id as the account. The macOS app has its own Swift Keychain code with the same service and account, so the two share entries.
- `apps/gpui` uses `dbcore` and `dbservice` directly (no FFI). It reads the same connection store and keyring entries as the macOS app. It's the Windows release (and the Linux app).
  - GPUI is pre-1.0. `gpui-kit` is pinned to an exact version and pins GPUI (`gpui-pre`) itself; upgrade both together.
  - On macOS, GPUI stops drawing windows that other windows cover. For screenshots in tests, use a window that floats above the others.
  - `RUST_LOG=info` shows the renderer GPUI picked and what it's doing.
- Platform folders come from `dbcore::paths`. Don't read `HOME` directly; the core also builds for Windows.
- After changing `crates/`, run `./scripts/build-core.sh` (`bundle-mac.sh` runs it for you). `Sources/DBCoreFFI` and `Frameworks/` are generated.
- Check work with `cargo test -p dbcore`, `./scripts/test-postgres.sh` and `(cd apps/macos && swift test)`; for the
  service, CLI and plugin also `cargo test -p dbservice -p dbear-cli` and `plugins/nvim/tests/run.sh`.
- `dbservice` and `dbear-cli` keep their integration tests in one test binary (`tests/it/main.rs` with modules):
  every test binary links all of dbcore. Do the same in new crates.
- Dev DB: `./scripts/dev-db.sh up|down|reset|psql`. Seed is in `dev/postgres/init.sql`.
- Use `nix develop`. Commit messages use `feat:`, `fix:`, `chore:` prefixes.

## Setup

```sh
nix develop                                       # Rust toolchain (Xcode provides Swift on macOS)
./scripts/dev-db.sh up                            # dev Postgres at localhost:54329/app_dev
./scripts/bundle-mac.sh && open build/dbear.app   # build and run
./scripts/run-gpui.sh [--samples] [--release]      # the GPUI app (Linux; runs on macOS for testing). --help for options
cargo run -p dbear-cli -- --help                  # the CLI (and `serve`, for editor plugins)
```

In debug builds, **File ▸ Add Sample Connections** adds `app_dev` plus some mock connections.
`bundle-mac.sh` signs with your "Apple Development" identity when you have one, so the Keychain's
"Always Allow" survives rebuilds.

Without Nix (it only pins the toolchain):

- **Rust**: [rustup](https://rustup.rs). `rustup show` in the repo root picks up `rust-toolchain.toml`.
- **macOS**: Xcode (Swift, `xcodebuild`, `lipo`, `codesign`) plus `pkg-config`. Cargo/`cc` should
  use Xcode's clang, the default outside the Nix shell.
- **Linux**: a C toolchain and `pkg-config`, plus GPUI's native deps, e.g. on Debian/Ubuntu:
  `apt install clang pkg-config libwayland-dev libxkbcommon-dev libvulkan-dev libgl1-mesa-dev
  libfontconfig1-dev libfreetype6-dev libssl-dev libasound2-dev libx11-dev libxcb1-dev
  libxcursor-dev libxi-dev libxrandr-dev`.

## Layout

```
crates/dbcore/      Rust core: models, drivers (Postgres, MySQL, SQLite, SQL Server, libSQL, D1, mock), connection store, SQL highlighting
crates/dbservice/   glue shared by the GPUI app and the CLI: stores + keychain → connections, caches, scripts, history, CRUD
crates/dbcore-ffi/  UniFFI bindings for Swift
crates/update/      self-update core (signed manifests, verified downloads) + `dbear-update` signing tool
apps/macos/         SwiftUI app
apps/gpui/          GPUI app: Windows release and Linux (gpui-kit pinned; also runs on macOS for development)
apps/cli/           dbear-cli: one-shot commands and `serve` (JSON-RPC over stdio or a socket; PROTOCOL.md)
plugins/nvim/       Neovim plugin (Lua, no deps) on `dbear-cli serve`
packaging/          Windows installer (NSIS), icon, and the update public key
scripts/            build, bundle, dev database, tests, release
vendor/             third-party crates patched for dbear (tiberius; see vendor/README.md)
```

## Tests

```sh
cargo test -p dbcore              # core (Postgres tests skip without a database)
cargo test -p dbservice           # service layer (SQLite files, mock driver, MemorySecretStore)
cargo test -p dbear-cli           # CLI and serve (stdio and socket) against SQLite files and the mock driver
plugins/nvim/tests/run.sh         # Neovim plugin, headless, against a real `dbear-cli serve` and a SQLite file
./scripts/test-postgres.sh        # core against the dev database
./scripts/test-linux.sh [--windows] [--gpui] # core on Linux in an Apple container (running dev DBs forwarded); --windows cross-checks x86_64-pc-windows-gnu, --gpui builds apps/gpui
./scripts/test-libsql.sh          # core against the dev libSQL server (container dbear-libsql)
./scripts/test-ssh.sh             # SSH tunnels: dev Postgres and MySQL through the dev SSH server (container dbear-ssh)
./scripts/test-sqlserver.sh       # core against the dev SQL Server (amd64 image under Rosetta, 4 GB)
(cd apps/macos && swift test)     # Swift ⇄ Rust bridge
cargo test -p dbear-update        # update signatures and downloads
./scripts/test-update-windows.sh  # Windows: install an old build, update it from a local feed (CI: .github/workflows/windows.yml)
./scripts/test-update-windows.sh --wine  # same from macOS under Wine (stops before the installer runs; see the script)
```

## Releases and updates

`./scripts/release-mac.sh 0.1.0` builds `build/dbear-0.1.0-macos-arm64.zip` (Developer ID signed,
notarized). `--publish` tags `v0.1.0`, pushes the tag and creates the GitHub release (needs a clean
tree and `gh`).

Windows: `.github/workflows/release-windows.yml` runs on the `v*` tag that `release-mac.sh` pushes and
adds `dbear-<v>-windows-<arch>-setup.exe`, the portable zip and `dbear-update-windows.json` to the same
release. Secrets: `DBEAR_UPDATE_PRIVATE_KEY` (required), `WINDOWS_CERTIFICATE` and
`WINDOWS_CERTIFICATE_PASSWORD` (optional Authenticode .pfx). `scripts/release-windows.sh` builds the
same files on a Windows machine. From macOS, `nix develop .#windows` has cargo-xwin and makensis to
cross-check (`cargo xwin check -p dbear-gpui --target x86_64-pc-windows-msvc`) and try the packaging
with `release-windows.sh <v> --cross-debug`. Release builds need Windows: GPUI compiles its HLSL
shaders with the SDK's `fxc.exe` there (debug builds compile them at runtime from the cargo registry).

The macOS app updates with [Sparkle](https://sparkle-project.org). The feed is the `appcast.xml` asset of
the latest GitHub release, written by `release-mac.sh` and signed with the EdDSA key in the release
Mac's keychain (public half: `DBEAR_SPARKLE_PUBLIC_KEY` in `bundle-mac.sh`; if empty, the updater
stays off). `scripts/test-update.sh` runs a full update against a local feed.

## Implementation notes

- Windows build: `apps/gpui/build.rs` embeds the icon (`packaging/windows/dbear.ico`, from
  `scripts/make-windows-icon.sh`) and version info, and sets `DBEAR_VERSION` (CI passes the tag's) and
  `DBEAR_UPDATE_PUBLIC_KEY` (from `packaging/update-public-key`). Release builds use the GUI subsystem;
  `.cargo/config.toml` links the MSVC runtime statically. `dbear.exe --version` / `--update` work without a window.
- Self-update (`crates/update`, `apps/gpui/src/update.rs`): each release has a signed manifest per platform
  family (`dbear-update-windows.json`: payload + ed25519 signature over a context string and the payload).
  It lists each artifact's size, SHA-256 and own signature; nothing is installed unless all match, and only
  newer versions are offered. The app reads `releases/latest/download/…` (`DBEAR_UPDATE_FEED` overrides it,
  still verified). Builds without a public key never check. Windows installs run the NSIS installer with
  `/S /UPDATE [/RELAUNCH] /D=<dir>`; it waits for dbear.exe to exit. A copy without `uninstall.exe` beside it
  (the portable zip) takes the manifest's `zip` artifact instead and replaces its own exe
  (`dbear_update::replace`: the running exe is renamed to `dbear.old.exe`, removed at the next launch;
  a restart passes `--updated` so the new process waits for the old one to quit). In a folder it can't
  write to, it only notifies. Settings live in `state.db` under `gpui.updates`. Platforms plug in an
  `update::Installer` (Linux: none yet, so it's a no-op there and on macOS). Debug builds:
  `DBEAR_UPDATE_DRY_RUN=1|portable` and `DBEAR_UPDATE_SHOW_DIALOG=1` try the flow on macOS.
- Connection store: SQLite at `~/Library/Application Support/dbear/dbear.db` (`$XDG_CONFIG_HOME/dbear/`
  on Linux), versioned with `PRAGMA user_version`. An old `connections.json` is imported once and
  renamed to `connections.json.migrated`. Override with `DBEAR_CONNECTIONS_FILE`. Secrets live in the
  keychain, never in that file.
- App state: `dbcore::state::StateStore`, `state.db` next to the connection store. Holds UI state as
  JSON under a key (the GPUI app's open tabs: `gpui.session`) and per-connection query history (newest
  1000 kept). It's a separate file so `dbear.db`'s schema version doesn't move: released apps refuse a
  newer connection store.
- SQL highlighting: tree-sitter with [DerekStride/tree-sitter-sql](https://github.com/DerekStride/tree-sitter-sql)
  (crate `tree-sitter-sequel`) in `dbcore::highlight`. The core returns spans; frontends pick colors.
- Table paging: the core sorts by the user's columns then the primary key (or ctid/rowid) so pages
  are stable, and uses keyset paging (`dbcore::keyset`) instead of `OFFSET`. Views, keyless MySQL and
  SQL Server tables and sort types that don't compare the way they sort fall back to `OFFSET`. SQL
  Server has no row values, so it seeks with the expanded `a > x or (a = x and …)` form (NULLs first).
- Filters: the core takes a raw `WHERE` filter (`RowQuery::filter`, completed by
  `complete::complete_filter`); the macOS app doesn't expose it right now. `dialect::normalize_filter`
  rejects a `;` between statements and is the only guard for MySQL, whose text protocol runs multiple
  statements. Postgres and SQLite prepare a single statement. SQL Server also requires balanced
  parentheses (`sqlserver::script::check_filter`).
- Structure: `Driver::describe_table`. Postgres DDL is rebuilt from the catalogs; MySQL uses
  `SHOW CREATE TABLE` and SQLite `sqlite_master`.
- Edits: `dbcore::edit::statements` generates the SQL. The driver runs it in one transaction on its
  own session and rolls back if a statement fails or an UPDATE/DELETE doesn't match exactly one row.
  Values are sent as string literals and cast by the database.
- Script results: drivers fill `QueryResult::origins` (the table column each result column reads:
  Postgres from `prepare`'s table OID and column number, MySQL from `org_table`/`org_name`, SQLite
  from rusqlite's `column_metadata`). `dbcore::results::ResultSources` turns origins and the tables'
  structures into links and editability by column index. Both apps use it for script grids, and
  for a table tab's links (`ResultSources::for_table`). A table is editable when its whole primary
  key is in the result exactly once. Saving doesn't re-run the script.
- Command palette (⌘K / Ctrl+K, everywhere in the window): `dbcore::palette` matches and ranks
  (fuzzy, word starts first; `>` commands, `#` tables, `@` connections; an empty query lists all but
  tables). It lists commands, open tabs, connections and their databases, and the tables of every
  target listed so far. Tab on a connection connects to it and narrows the palette to its tables
  (a chip in the field, a spinner while they load); Backspace in the empty field goes back. GPUI's
  palette uses its own search field in the `Command`'s header so the chip can sit in it. macOS: `AppCommand` is both the menus and the palette's commands,
  `CommandPalette.swift` the overlay, `DBKit.PaletteSearch` the core. GPUI: `palette.rs` on
  gpui-kit's `Command` in a dialog; `menus::palette_commands` lists the commands, and commands are
  sent to what had focus before (left out when nothing there handles them).
- Copy/inspect: `dbcore::export::format_rows`; `export::pretty_json` keeps key order and digits.
- Users & roles: `dbcore::access` generates the SQL, run in one transaction where the database allows
  (Postgres). Postgres privileges are listed for the connection's current database. Access levels
  (`access::DatabaseLevel`) cover every schema, all tables and sequences, and default privileges for
  the roles that own objects there; they run in their database over a connection of their own. MySQL
  levels are privileges on `db.*`. Passwords: `access::generate_password`.
- Turso / libSQL: Hrana 3 over HTTP (`dbcore::libsql`, reqwest + rustls/ring, no libSQL C library).
  Each call runs on its own short-lived stream, so a transaction a script leaves open is rolled back
  when it ends. Cancel stops the request, but a running statement may still finish on the server.
- Cloudflare D1: REST API (`dbcore::d1`, `POST /accounts/{account}/d1/database/{id}/raw`; `DBEAR_D1_API`
  overrides the base URL for tests). No new config fields: `host` is the account ID (empty: the one
  account the token sees), `database` the name or UUID, `password` the API token (D1 Read / D1 Edit);
  URL `d1://<account>/<database>?token=…`. SQL and catalog are the libSQL driver's. Each request is one
  transaction and `BEGIN`/`COMMIT` are refused, so a script runs as one batch (all or nothing) and edits
  run as one batch with a one-row check after each statement. Table pages select `quote(col)` and parse
  the literals back (the API's JSON rounds big integers and turns blobs into arrays); script results
  stay JSON. Results arrive whole, no row counts, `_cf_*` tables hidden. `list_databases` is the
  account's D1 databases; there's no SSH and no New Database. `d1::discover::find(dir)` reads a Workers
  project's wrangler.json/jsonc/toml `d1_databases` and finds the local `wrangler dev` copies (SQLite
  files under `.wrangler/state/v3/d1/miniflare-D1DatabaseObject/`); neither app has an import UI for it yet.
  "Sign in with Cloudflare" (`d1::oauth`, both editors): OAuth Authorization Code + PKCE with dbear's public
  client (`oauth::CLIENT_ID`, registered in Fausto's account with redirect `http://127.0.0.1:38257/oauth/callback`;
  still private, so only that account's members can sign in until it's made public). `SignIn` listens on the
  redirect and returns a secret, `cloudflare-oauth:<json>` (grant id, access and refresh token, expiry), stored
  as the connection's password; `user` holds the account's name so editors know it's signed in without reading
  the keychain. The D1 client gets access tokens from `oauth::access_token`: grants are shared in the process by
  grant id, refreshed 5 minutes before expiry or after a 401, and the rotated secret is saved back through the
  `SecretSync` each app installs at launch (GPUI: `sync_with_store`; macOS: `CloudflareSignIn.installSecretSync`).
  `d1::accounts` / `d1::databases` fill the editors' pickers. Several processes (the apps, `dbear-cli`) may hold
  the same grant: a refresh takes a file lock per grant (`<app data>/dbear/locks/oauth-<sha256 prefix>.lock`, polled
  with `try_lock`, 45 s timeout, then it goes ahead unlocked), re-reads the stored secret through `SecretSync` and
  adopts a copy another process already rotated instead of spending a stale refresh token. Lock files are never
  deleted; unit tests use `<tmp>/dbear-test-locks`. `DBEAR_CF_OAUTH` / `DBEAR_CF_OAUTH_CLIENT_ID`
  override the endpoints and client. Try it with `cargo run -p dbcore --example d1_oauth -- --url d1:///<database>`.
- Dump / restore: `dbcore::dump` writes from one consistent snapshot, streaming to `<file>.partial`
  until done. Turso dumps use the SQLite format; SQL Server dumps are T-SQL with `GO` batches,
  skipping what the file header lists. `dbcore::restore` also handles `COPY … FROM stdin` blocks.
  CLI: `cargo run -p dbcore --example dump -- dump <url> out.sql.gz`. Round trips are tested in
  `crates/dbcore/tests/dump_*.rs` (fixtures in `dev/dump/`).
- SSH tunnels: `dbcore::ssh` ([russh](https://github.com/warp-tech/russh) on ring). A connection
  with `ssh` gets a `TunneledDriver`: it opens the tunnel on first use (and again after the SSH
  session drops), listens on a free local port and forwards each connection to `host:port` as seen
  from the SSH server (`direct-tcpip`). Drivers connect to `127.0.0.1:<forwarded_port>` but keep
  `host` for TLS (Postgres `hostaddr`, MySQL hostname override). Dumps and restores open their own
  tunnel (`ssh::route`). Host keys: the user's `~/.ssh/known_hosts` is read, never written; unknown
  hosts are trusted once and recorded in `<app data>/dbear/known_hosts` (`DBEAR_KNOWN_HOSTS`
  overrides it, for tests). The SSH password or passphrase is a keychain entry of its own, account
  `<id>:ssh` (`secrets::ssh_account`). Dev server: `./scripts/dev-db.sh up ssh` (keys in `dev/ssh`).
- SQL Server: [tiberius](https://github.com/prisma/tiberius), vendored with a small patch
  (`vendor/README.md`): rustls on ring, row counts for scripts, exact MONEY. Each `GO` batch runs on
  the script session, so temp tables and `SET` options persist. SSL "Disable" (shown as "Login Only")
  still encrypts the login.
- Service layer (`crates/dbservice`): `Service` (`Clone + Send + Sync`, async methods executor-agnostic like
  `Connection`; keychain and SQLite work runs through `dbservice::unblock`). `Service::open(Options)` opens the
  connection store (`DBEAR_CONNECTIONS_FILE`), `state.db` beside it and the keychain (`Secrets::Keyring | Store |
  Disabled`; `DBEAR_NO_KEYRING=1` turns the keychain off for tests and sandboxes), and installs
  `oauth::sync_with_store`. `open_lenient` never fails and lists what didn't open in `problems()` (GPUI's notices).
  `find(spec)`: an id, an exact name (trimmed, case-insensitive; ambiguous names are an error listing the ids) or a
  URL (id `url:<hex>`, kept in memory, never saved, password from the URL). `Target { connection_id, database }`:
  `database` is always a name (`default_database()` when it's the connection's own); with none asked for, a
  connection that browses databases opens its `last_database`. `connection(&target)` fills both secrets (not the
  SSH one for Agent auth) and caches one `Connection` per target; `new_connection` is uncached, for sessions of
  their own. Schemas, database lists and completion catalogs are cached per target and dropped by `schema_changed`
  (scripts that `dialect::changes_schema`), restores and `forget`. `run_script` → `ScriptOutcome` (result,
  duration, `schema_changed`, sources): 10 000-row cap, history as the apps record it (rows affected or rows
  returned; cancelled runs aren't recorded). `save` writes the store, then the keychain (the SSH secret is deleted
  without a tunnel or with Agent auth); `delete` also removes both keychain entries, the history and the caches.
  `rename` changes only the name (`ConnectionStore::rename`: trimmed, empty → `default_name()`, duplicates allowed;
  `last_database`, position and columns another process changed stay); URL and sample connections are refused,
  nothing cached is dropped.
  `table_editing` / `preview_table_changes` / `apply_table_changes` (`editing.rs`): what a client may edit in a
  table (`TableEditing`: read-only reason, key, binary/read-only columns, links by column name), checked before any
  SQL, applied in one transaction, optionally only if the SQL is what the user reviewed.
  `dump_options` / `restore_options` normalize per kind (Insert unless Postgres, `create_database` MySQL only,
  MySQL restores outside a transaction). GPUI keeps paging, edit staging and the session JSON.
- CLI (`apps/cli`, binary `dbear-cli`; the GPUI binary is `dbear`): `ls`, `dbs`, `tables`, `describe`, `rows`,
  `query`, `dump`, `restore`, `history`, `url`, `serve`. `<conn>` is a name, id or URL. Every command is a JSON-RPC
  client of the server code (`server.rs`): over the socket of a running `serve --socket` for the same store
  (checked with `server.info`'s `store_path` and protocol), else an in-process session on an in-memory pipe
  (`--no-server` forces it). Output: an aligned table on a TTY, TSV when piped, `--format` for `dbcore::export`'s
  formats, `--json` for the serde models. `query` records history only with `--history` (scripts would push the
  user's queries out of the newest 1000); Ctrl-C sends `query.cancel` (exit 130). Exit codes: 1 errors, 2 usage.
- `serve`: JSON-RPC 2.0, one JSON message per line, over stdio (one client) or `--socket [PATH]` (Unix only; default
  `serve.sock` beside the connection store, `DBEAR_SOCKET` overrides; mode 0600; a stale file is replaced, a live
  server refused; removed on exit). The `Service` (stores, keychain, OAuth grants, one metadata connection per target
  and its caches) is shared; each client gets its own `query.run` sessions (`new_connection`), so temp tables,
  transactions and cancel never cross clients, and a socket client that hangs up has its work cancelled. Requests
  run concurrently. A failing statement is a successful `query.run` with `{"error": …}`; RPC errors carry the
  serialized `dbcore::Error` in `data`. Dumps and restores are jobs (`*.start` answers with a job id, then
  `*.progress` / `*.finished` notifications; `job.cancel`). The store is re-read at most every 2 s (a connection
  only renamed keeps its sessions and caches); `connections.rename` is the server's only store write. Everything is in
  `apps/cli/PROTOCOL.md` (protocol version 1: new methods and fields don't bump it).
- Neovim plugin (`plugins/nvim`, Neovim 0.10+, no deps): `dbear.rpc` is the JSON-RPC client (no UI code; starts
  `dbear-cli serve` on the first request and again after it exits), `dbear.render` turns results into lines and
  highlights, `dbear` (init.lua) has the commands (`:Dbear`, `:DbearDatabase`, `:DbearRun`, `:DbearTables`,
  `:DbearDescribe`, `:DbearCancel`, `:DbearHistory`), the `dbear://results` split and the `omnifunc` (a synchronous
  `complete` call; offsets are UTF-8 bytes on both sides). A buffer's connection is `b:dbear_connection`. It records
  history (`record_history = true` by default).
  The browser (`dbear.browse.*`, oil.nvim style; `:DbearOpen!`, or `:e dbear://…`): `dbear://` lists connections,
  `dbear://<conn>/` its databases or tables, `dbear://<conn>/[<db>/]<schema>.<table>[?where=…&sort=…]` is the table's
  rows as an editable buffer (`browse.grid`). Each line starts with a concealed row id (`/007:`) mapping to the row
  as loaded (its key values); `browse.diff` compares the text with the loaded rows: a missing id is a DELETE, a
  changed cell an UPDATE of that cell only, a repeated id (yyp) or a line without one an INSERT. Cells are separated
  by `|` with lossless escapes (`browse.cells`: `\\ \| \n \t \r \s \xHH`, whole-cell `\N` NULL, `\D` DEFAULT,
  `\e` ''); values wider than `browse.max_width` are cut (`…`) and refused if edited in the line: `<CR>` edits a
  cell's whole value in a buffer of its own. `:w` sends `RowChange`s to `table.changes.preview`, shows the SQL in a
  float, then `table.changes.apply` with the reviewed SQL (Lua never builds SQL). Loading rows never leaves an undo
  step (undoing it would look like deletes): the history restarts when a page loads; realigning joins the edit's
  undo step and is skipped on undo/redo states. What can be edited comes from `dbservice::TableEditing`
  (`table.editable`), which also checks the changes server-side.
  The mirror (`dbear.tree.*`; `:DbearOpen` opens its root, `:DbearSync[!]`, `:DbearFind`, `:DbearGrep`): real
  folders `<root>/<connection>/<database>/<schema>/<table>.table|.view` (root `tree.root`, default
  `stdpath("cache")/dbear/tree`; always four levels: one database folder for connections that don't browse
  databases, SQLite's named after its file) for oil, netrw, telescope… Table files hold a header line
  (`#dbear-mirror v1 {json target}`) and the column list (`columns.list`, one call per database); a `BufReadCmd`
  on them opens `browse.grid` instead (`-` goes to the folder). Nothing is listed at startup: a `BufEnter` on a
  mirror folder (oil://, netrw, any directory buffer) lists it once per session (`tree.sync`: root =
  `connections.list`, connection = `databases.list`, database = `schemas.list` + `columns.list`); failures keep
  the tree and show `dbear: ✗ …` under the folder's first line. `tree.names` escapes names reversibly (`%XX`,
  `%-` empty, `%~N` for siblings the file system can't tell apart, `%+hash` cut), but entries map to targets
  through `<root>/.dbear-mirror.json` (`tree.manifest`) and the headers, never by parsing names. Folders are
  555 and files 444 (a file is 644 while its rows buffer is loaded: Vim refuses `:w` otherwise); every disk
  change goes through `tree.fs`, which refuses anything outside the root (symlinks resolved, never followed)
  and a non-empty root without a manifest. URL connections aren't mirrored.
  Renaming a connection folder at the root renames the connection (`tree.rename`, default on): the manifest records
  each connection folder's identity (`ident`: inode + birth time), so the root listing (`sync.root`) finds a folder
  under a new name, maps the name back (`names.parse`: `%XX` reversed, a trailing `%~N` dropped) and calls
  `connections.rename` before it reconciles; a failure leaves the store alone and the reconcile moves the folder
  back. Anything else at the root is undone there (strays removed, a deleted connection folder re-created; the store
  is never touched). The root stays 555: oil's root buffer is made modifiable by wrapping
  `oil.adapters.files.is_modifiable`, and `User OilActionsPre` makes the root and the renamed folders writable
  (macOS needs a folder writable to rename it) only when every mirror action of the batch is an in-place rename of
  a connection folder to a free name; `OilActionsPost` (or a 30 s timer) makes them read-only again and lists the
  root. Buffers below a renamed folder follow it (`tree.moved`: rows buffers keep pending edits; hidden oil buffers
  are dropped). `:DbearRename [name]` goes through the same `connections.rename` + root listing.
