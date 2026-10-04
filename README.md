# dbear

<img src="assets/logo.svg" width="96" alt="dbear">

Native database client. The macOS app is SwiftUI, a Linux app in GPUI is planned. Both sit on a
shared Rust core.

## Quick start

```sh
nix develop                                       # Rust toolchain (Xcode provides Swift on macOS)
./scripts/dev-db.sh up                            # dev Postgres at localhost:54329/app_dev
./scripts/bundle-mac.sh && open build/dbear.app   # build and run
```

In debug builds, **File ▸ Add Sample Connections** adds `app_dev` plus some mock connections.

### Releases

`./scripts/release-mac.sh 0.1.0` builds `build/dbear-0.1.0-macos-arm64.zip`. Add `--publish` to tag
`v0.1.0`, push the tag and create the GitHub release with the zip attached (needs a clean tree and `gh`).

### Updates

The app updates itself with [Sparkle](https://sparkle-project.org): it checks daily, downloads and
verifies updates in the background and installs them on quit, without popups (a "Restart to
Update" item appears in the app menu). Settings (⌘,) turns this off. The feed is the `appcast.xml`
asset of the latest GitHub release, written by `release-mac.sh` and signed with the EdDSA key in
the release Mac's keychain (public half: `DBEAR_SPARKLE_PUBLIC_KEY` in `bundle-mac.sh`; if it's empty,
the updater stays off). `scripts/test-update.sh` runs a full update against a local feed.

### Without Nix

You don't need Nix; it just pins the exact toolchain. Install equivalents yourself:

- **Rust**: install via [rustup](https://rustup.rs). `rustup show` in the repo root picks up
  `rust-toolchain.toml` (stable channel + `rustfmt`, `clippy`, `rust-src`, `rust-analyzer`).
- **macOS**: Xcode (for Swift, `swift build`, `xcodebuild`, `lipo`, `codesign`) plus `pkg-config`
  (`brew install pkg-config`). Cargo/`cc` should use Xcode's clang, which is the default outside
  the Nix shell.
- **Linux**: a C toolchain (`clang` or `gcc`) and `pkg-config`, plus GPUI's native deps:
  `wayland`, `libxkbcommon`, a Vulkan loader, `libGL`/Mesa, `fontconfig`, `freetype`, `openssl`,
  `alsa-lib`, and X11 libs (`libX11`, `libxcb`, `libXcursor`, `libXi`, `libXrandr`) — e.g. on
  Debian/Ubuntu: `apt install clang pkg-config libwayland-dev libxkbcommon-dev libvulkan-dev
  libgl1-mesa-dev libfontconfig1-dev libfreetype6-dev libssl-dev libasound2-dev libx11-dev
  libxcb1-dev libxcursor-dev libxi-dev libxrandr-dev`.

Then skip `nix develop` and run the same commands as above (`./scripts/dev-db.sh up`,
`./scripts/bundle-mac.sh`, `cargo test -p dbcore`, ...) directly.

## Layout

```
crates/dbcore/      Rust core: models, drivers (Postgres, MySQL, SQLite, SQL Server, mock), connection store, SQL highlighting
crates/dbcore-ffi/  UniFFI bindings for Swift
apps/macos/         SwiftUI app
apps/linux/         GPUI app (todo)
scripts/            build, bundle, dev database, tests
vendor/             third-party crates patched for dbear (tiberius; see vendor/README.md)
```

## Tests

```sh
cargo test -p dbcore              # core (Postgres tests skip without a database)
./scripts/test-postgres.sh        # core against the dev database
./scripts/test-libsql.sh          # core against the dev libSQL server (container dbear-libsql)
./scripts/test-sqlserver.sh       # core against the dev SQL Server (amd64 image under Rosetta, 4 GB)
(cd apps/macos && swift test)     # Swift ⇄ Rust bridge
```

## Notes

- Connections are saved in a SQLite database, `~/Library/Application Support/dbear/dbear.db` (or
  `$XDG_CONFIG_HOME/dbear/` on Linux), versioned with `PRAGMA user_version`. An old
  `connections.json` is imported once and renamed to `connections.json.migrated`. Override the
  path with `DBEAR_CONNECTIONS_FILE`.
- Passwords go in the system keychain, never in that file.
- SQL highlighting uses tree-sitter with [DerekStride/tree-sitter-sql](https://github.com/DerekStride/tree-sitter-sql)
  (crate `tree-sitter-sequel`) in `dbcore::highlight`. The core returns spans and each frontend picks the colors.
- Table tabs sort on the server (click a header: ascending → descending → off), and switch
  between Data and Structure from the toolbar (⌥⌘1 / ⌥⌘2). The core also takes a raw `WHERE`
  filter (`RowQuery::filter`, completed by `complete::complete_filter`); the macOS app doesn't
  expose it right now. The core sorts by the primary key (or ctid/rowid) after the
  user's columns, so pages stay stable. Pages are fetched with keyset paging (`dbcore::keyset`: `where (sort keys) > (last row's)`
  instead of `OFFSET`), so deep pages load as fast as the first; views, keyless MySQL and SQL Server tables and sort types
  that don't compare the way they sort fall back to `OFFSET`. SQL Server has no row values, so it seeks with the expanded
  `a > x or (a = x and …)` form (NULLs sort first). The core also rejects a filter with a `;` between statements:
  `dialect::normalize_filter` is the only guard for MySQL, whose text protocol runs multiple
  statements. Postgres and SQLite also prepare a single statement.
- Structure (⌥⌘2) comes from `Driver::describe_table`: columns with defaults and comments, the
  primary key in key order, indexes, foreign keys and DDL. Postgres DDL is rebuilt from the
  catalogs; MySQL uses `SHOW CREATE TABLE` and SQLite `sqlite_master`.
- Rows of tables with a primary key are editable: double-click or Return edits a cell, Tab moves to
  the next one, "+" or ⌥⌘N adds a row, and ⌫ deletes rows. Edits stay pending until ⌘S, which shows the
  exact SQL (`dbcore::edit::statements`) before running it. The driver runs it in one
  transaction on a session of its own. It rolls back if any statement fails, or if an
  UPDATE/DELETE doesn't match exactly one row (the row changed since it was loaded). Values
  are sent as string literals and cast by the database. Views, keyless tables and binary
  columns are read-only.
- Turso / libSQL connections (`libsql://db-org.turso.io?authToken=…`) speak Hrana 3 over HTTP
  (`dbcore::libsql`, reqwest + rustls/ring, no libSQL C library). The auth token is stored in the
  keychain like a password. Each call runs on its own short-lived stream, so a transaction a script
  leaves open is rolled back when that script ends. Row counts are never computed, because Turso bills
  rows read. Cancel stops the request, but a statement already running on the server can still finish.
  Local libSQL files are SQLite files: open them as SQLite.
- **Dump / restore** (connection or tables-list menus): `dbcore::dump` writes plain SQL (optionally
  gzipped) that `psql`, `mysql` and `sqlite3` can load, from one consistent snapshot, streaming to
  `<file>.partial` until done. Turso dumps use the SQLite format; SQL Server dumps are T-SQL with `GO`
  batches (sqlcmd/SSMS), skipping what the file header lists (permissions, sequences, synonyms…).
  `dbcore::restore` runs such scripts, also `pg_dump`/`mysqldump`/`sqlite3 .dump` plain output,
  including `COPY … FROM stdin` blocks. Postgres dumps skip owners and grants
  (`pg_dump --no-owner --no-privileges`). CLI for testing: `cargo run -p dbcore --example dump -- dump <url> out.sql.gz`.
  Round trips are tested in `crates/dbcore/tests/dump_*.rs` (fixtures in `dev/dump/`).
- `bundle-mac.sh` signs with your "Apple Development" identity when you have one, so the
  Keychain's "Always Allow" survives rebuilds.
- SQL Server uses [tiberius](https://github.com/prisma/tiberius), vendored with a small patch
  (`vendor/README.md`): rustls on ring, row counts for scripts, exact MONEY. Scripts are split on
  `GO` lines, and each batch runs on the script session, so temp tables and `SET` options persist.
  A T-SQL batch can run several statements with no `;` between them, so the driver also requires a
  table filter's parentheses to balance (`sqlserver::script::check_filter`). SSL "Disable" (shown as
  "Login Only") still encrypts the login, as SQL Server always does. Only SQL logins are supported;
  Windows and Azure AD authentication are not yet.
