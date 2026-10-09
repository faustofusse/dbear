# dbear

<img src="assets/logo.svg" width="96" alt="dbear">

A fast, native database client for macOS. It works with **PostgreSQL**, **MySQL**, **SQLite**,
**SQL Server** and **Turso / libSQL**. A Linux version is planned.

## Install

Download the latest `dbear-*-macos-arm64.zip` from
[Releases](https://github.com/faustofusse/dbear/releases/latest), unzip it and move `dbear.app` to
Applications. It needs macOS 15 or later on Apple silicon.

dbear updates itself. It checks once a day, downloads updates in the background and installs them
when you quit. When an update is ready, **Restart to Update** appears in the app menu. You can turn
this off in Settings (⌘,).

## Connecting

**File ▸ New Connection…** (⇧⌘N). Paste a connection URL such as
`postgres://user:password@host:5432/database` to fill in the fields, or enter them by hand. For
Turso, paste the `libsql://…?authToken=…` URL. Local libSQL files are SQLite files, so open them
as SQLite.

- **Import from DBeaver…** brings in your existing DBeaver connections.
- **Copy URL** in a connection's context menu copies its URL, including the saved password.
- Pick the database from the title of the tables column. dbear remembers the last database you
  used on each connection. **New Database…** creates one.
- SQL Server supports SQL logins only. Windows and Azure AD authentication aren't supported yet.

Passwords and auth tokens are stored in the macOS Keychain. Everything else is kept in
`~/Library/Application Support/dbear/dbear.db`.

## Features

**Browse tables.** Click a column header to sort it (ascending, then descending, then off). Deep
pages load as fast as the first one. Switch between **Data** and **Structure** (⌥⌘1 / ⌥⌘2).
Structure shows columns, the primary key, indexes, foreign keys and the table's DDL.

**Follow foreign keys.** Open the row that a foreign-key cell points to, or jump from a row to
the rows in other tables that reference it.

**Edit rows.** Tables with a primary key are editable. Double-click a cell or press Return to edit
it, Tab moves to the next cell, ⌥⌘N adds a row and ⌫ deletes the selected rows. Changes stay
pending until you save. ⌘S saves them, and ⇧⌘S shows the exact SQL first. All changes run in one
transaction. If any statement fails, or if a row changed since you loaded it, nothing is saved.
Views, tables without a primary key and binary columns are read-only.

**Copy and inspect.** ⌘C copies the selected rows as TSV, ⇧⌘C copies them with headers and ⌥⌘C
copies the focused value. The context menu also copies rows as CSV, JSON, a Markdown table or
`INSERT` statements. The inspector (⌥⌘I) shows the full value, pretty-prints JSON and lets you edit
the value.

**Run SQL.** ⌘T opens a new SQL script with syntax highlighting. ⌘↩ runs the script, or only the
selected text, and ⌘. stops it. ⇧⌘↩ runs it in a new results tab instead, and the script's
**Open in New Tab** button moves the rows it shows to one, so the next run doesn't replace them.
Results tabs can be re-run (⌘↩) but aren't reopened at launch. For SQL Server, scripts are split
on `GO` lines. The **History** menu above the editor lists the connection's recent queries; picking
one adds it to the script.

**Tabs reopen at launch**, with the connection you had selected and your scripts' text. Tables load
when you first show them.

On Postgres, MySQL and SQLite, script results work like a table's rows wherever a column is read
straight from a table, including through joins and aliases. Foreign-key cells link to their row,
and rows list the tables that reference them. You can edit cells of a table whose primary key is
in the results, and save them like table edits. Deleting rows works when every column comes from
one table. Computed columns, and tables whose primary key you didn't select, stay read-only, and
the inspector says why. Saving doesn't re-run the script, because a script can do more than
select.

**Dump and restore.** **Dump Database…** (also available per schema or per table) writes a plain
SQL file, optionally gzipped, that `psql`, `mysql`, `sqlite3` or `sqlcmd` can load.
**Restore from File…** runs dbear dumps as well as plain output from `pg_dump`, `mysqldump` and
`sqlite3 .dump`. Postgres dumps leave out owners and grants.

**Users and roles** (Postgres and MySQL). ⇧⌘U, or the Tables | Users switch, lists roles and
accounts. You can create, edit and drop them, generate passwords, and grant or revoke privileges
on databases, schemas and tables. You can also set an access level for each database: connect
only, read only, read and write, or schema changes. dbear shows the SQL before it runs, with
passwords masked. Only privileges granted directly to a role are listed; inherited ones aren't.

**Turso.** dbear never shows row counts for Turso databases, because Turso bills by rows read.

## Keyboard shortcuts

| | |
|---|---|
| ⇧⌘N / ⇧⌘E | New / edit connection |
| ⌘T | New SQL script |
| ⌘↩ / ⌘. | Run / stop script |
| ⇧⌘↩ | Run script in a new results tab |
| ⌥⌘1 / ⌥⌘2 | Data / Structure |
| ⌥⌘N | Add row |
| ⌘S / ⇧⌘S | Save changes / review changes |
| ⌥⌘I | Value inspector |
| ⇧⌘U | Users & Roles |
| ⇧⌘[ / ⇧⌘] | Previous / next tab |
| ⌘+ / ⌘- / ⌘0 | Zoom in / out / actual size |

## Building from source

```sh
nix develop                                       # or install Rust (rustup) and Xcode yourself
./scripts/bundle-mac.sh && open build/dbear.app
```

Contributor notes are in [AGENTS.md](AGENTS.md).
