# AGENTS.md

- Database logic (drivers, decoding, SQL, connection storage, URL parsing) belongs in `crates/dbcore`. It has no FFI or UI deps.
- `crates/dbcore-ffi` is the only FFI surface. Only `apps/macos/Sources/DBKit/RustDriver.swift` imports `DBCoreFFI`.
- Rows cross the FFI boundary in pages (`QueryResult`), never one cell at a time.
- Passwords never touch disk. `dbcore::secrets` keeps them in the OS store (feature `os-keyring`: Keychain, Secret Service, Credential Manager), using service `ar.fausto.dbear.connection` and the connection id as the account. The macOS app has its own Swift Keychain code with the same service and account, so the two share entries.
- `apps/gpui` uses `dbcore` directly (no FFI). It reads the same connection store and keyring entries as the macOS app. It's the Windows release (and the Linux app).
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
crates/update/      self-update core (signed manifests, verified downloads) + `dbear-update` signing tool
apps/macos/         SwiftUI app
apps/gpui/          GPUI app: Windows release and Linux (gpui-kit pinned; also runs on macOS for development)
packaging/          Windows installer (NSIS), icon, and the update public key
scripts/            build, bundle, dev database, tests, release
vendor/             third-party crates patched for dbear (tiberius; see vendor/README.md)
```

## Tests

```sh
cargo test -p dbcore              # core (Postgres tests skip without a database)
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
  (the portable zip) only notifies. Settings live in `state.db` under `gpui.updates`. Platforms plug in an
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
