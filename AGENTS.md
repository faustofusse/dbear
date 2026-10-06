# AGENTS.md

- Database logic (drivers, decoding, SQL, connection storage, URL parsing) belongs in `crates/dbcore`. It has no FFI or UI deps.
- `crates/dbcore-ffi` is the only FFI surface. Only `apps/macos/Sources/DBKit/RustDriver.swift` imports `DBCoreFFI`.
- Rows cross the FFI boundary in pages (`QueryResult`), never one cell at a time.
- Passwords never touch disk. `dbcore::secrets` keeps them in the OS store (feature `os-keyring`: Keychain, Secret Service, Credential Manager), using service `ar.fausto.dbear.connection` and the connection id as the account. The macOS app has its own Swift Keychain code with the same service and account, so the two share entries.
- `apps/gpui` uses `dbcore` directly (no FFI). It reads the same connection store and keyring entries as the macOS app.
  - GPUI is pre-1.0. `gpui-kit` is pinned to an exact version and pins GPUI (`gpui-pre`) itself; upgrade both together.
  - On macOS, GPUI stops drawing windows that other windows cover. For screenshots in tests, use a window that floats above the others.
  - `RUST_LOG=info` shows the renderer GPUI picked and what it's doing.
- Platform folders come from `dbcore::paths`. Don't read `HOME` directly; the core also builds for Windows.
- After changing `crates/`, run `./scripts/build-core.sh` (`bundle-mac.sh` runs it for you). `Sources/DBCoreFFI` and `Frameworks/` are generated.
- Check work with `cargo test -p dbcore`, `./scripts/test-postgres.sh` and `(cd apps/macos && swift test)`.
- Dev DB: `./scripts/dev-db.sh up|down|reset|psql`. Seed is in `dev/postgres/init.sql`.
- Use `nix develop`. Commit messages use `feat:`, `fix:`, `chore:` prefixes.

## Setup

```sh
nix develop                                       # Rust toolchain (Xcode provides Swift on macOS)
./scripts/dev-db.sh up                            # dev Postgres at localhost:54329/app_dev
./scripts/bundle-mac.sh && open build/dbear.app   # build and run
./scripts/run-gpui.sh [--samples] [--release]      # the GPUI app (Linux; runs on macOS for testing). --help for options
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
crates/dbcore/      Rust core: models, drivers (Postgres, MySQL, SQLite, SQL Server, libSQL, mock), connection store, SQL highlighting
crates/dbcore-ffi/  UniFFI bindings for Swift
apps/macos/         SwiftUI app
apps/gpui/          GPUI app for Linux (gpui-kit pinned; also runs on macOS for development)
scripts/            build, bundle, dev database, tests, release
vendor/             third-party crates patched for dbear (tiberius; see vendor/README.md)
```

## Tests

```sh
cargo test -p dbcore              # core (Postgres tests skip without a database)
./scripts/test-postgres.sh        # core against the dev database
./scripts/test-linux.sh [--windows] [--gpui] # core on Linux in an Apple container (running dev DBs forwarded); --windows cross-checks x86_64-pc-windows-gnu, --gpui builds apps/gpui
./scripts/test-libsql.sh          # core against the dev libSQL server (container dbear-libsql)
./scripts/test-sqlserver.sh       # core against the dev SQL Server (amd64 image under Rosetta, 4 GB)
(cd apps/macos && swift test)     # Swift ⇄ Rust bridge
```

## Releases and updates

`./scripts/release-mac.sh 0.1.0` builds `build/dbear-0.1.0-macos-arm64.zip` (Developer ID signed,
notarized). `--publish` tags `v0.1.0`, pushes the tag and creates the GitHub release (needs a clean
tree and `gh`).

The app updates with [Sparkle](https://sparkle-project.org). The feed is the `appcast.xml` asset of
the latest GitHub release, written by `release-mac.sh` and signed with the EdDSA key in the release
Mac's keychain (public half: `DBEAR_SPARKLE_PUBLIC_KEY` in `bundle-mac.sh`; if empty, the updater
stays off). `scripts/test-update.sh` runs a full update against a local feed.

## Implementation notes

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
- Copy/inspect: `dbcore::export::format_rows`; `export::pretty_json` keeps key order and digits.
- Users & roles: `dbcore::access` generates the SQL, run in one transaction where the database allows
  (Postgres). Postgres privileges are listed for the connection's current database. Access levels
  (`access::DatabaseLevel`) cover every schema, all tables and sequences, and default privileges for
  the roles that own objects there; they run in their database over a connection of their own. MySQL
  levels are privileges on `db.*`. Passwords: `access::generate_password`.
- Turso / libSQL: Hrana 3 over HTTP (`dbcore::libsql`, reqwest + rustls/ring, no libSQL C library).
  Each call runs on its own short-lived stream, so a transaction a script leaves open is rolled back
  when it ends. Cancel stops the request, but a running statement may still finish on the server.
- Dump / restore: `dbcore::dump` writes from one consistent snapshot, streaming to `<file>.partial`
  until done. Turso dumps use the SQLite format; SQL Server dumps are T-SQL with `GO` batches,
  skipping what the file header lists. `dbcore::restore` also handles `COPY … FROM stdin` blocks.
  CLI: `cargo run -p dbcore --example dump -- dump <url> out.sql.gz`. Round trips are tested in
  `crates/dbcore/tests/dump_*.rs` (fixtures in `dev/dump/`).
- SQL Server: [tiberius](https://github.com/prisma/tiberius), vendored with a small patch
  (`vendor/README.md`): rustls on ring, row counts for scripts, exact MONEY. Each `GO` batch runs on
  the script session, so temp tables and `SET` options persist. SSL "Disable" (shown as "Login Only")
  still encrypts the login.
