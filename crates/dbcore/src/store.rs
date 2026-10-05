//! Saved connections: one SQLite database shared by every frontend.
//!
//! Passwords are never written here (there is no column for them). Frontends keep them in the
//! platform keychain (Keychain on macOS, libsecret on Linux) keyed by connection id, and set
//! `ConnectionConfig::password` right before connecting.
//!
//! The schema is versioned with `PRAGMA user_version`; [`MIGRATIONS`] brings older files up to
//! date. Several app instances can share the file: every write is its own transaction and the
//! in-memory list is reloaded from disk afterwards.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind, SslMode};

/// File name inside the app's config folder.
pub const DATABASE_FILE: &str = "dbear.db";
/// The JSON store used before SQLite. Imported once, then renamed to `connections.json.migrated`.
const LEGACY_JSON_FILE: &str = "connections.json";

/// Schema migrations; entry `n` upgrades `user_version` `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[
    // 1: connections, in user order.
    "create table connections (
        id                 text primary key,
        position           integer not null,
        name               text not null,
        grp                text not null default '',
        kind               text not null,
        host               text not null default '',
        port               integer,
        database           text not null default '',
        user               text,
        ssl_mode           text not null,
        show_all_databases integer not null default 1
    ) strict;
    create index connections_position on connections (position);",
    // 2: the database last browsed on each connection, reopened next time.
    "alter table connections add column last_database text;",
];

fn storage(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Storage(format!("{}: {e}", path.display()))
}

/// `~/Library/Application Support/dbear/dbear.db` on macOS,
/// `$XDG_CONFIG_HOME/dbear/dbear.db` (or `~/.config/…`) elsewhere.
pub fn default_path() -> Option<PathBuf> {
    Some(config_dir("dbear")?.join(DATABASE_FILE))
}

/// The app was called DBGui before; its folder is moved over on first use.
const LEGACY_DIR_NAME: &str = if cfg!(target_os = "macos") { "DBGui" } else { "dbgui" };

fn config_dir(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.map(|h| h.join(".config")))?
    };
    Some(base.join(name))
}

/// Moves `legacy` to `current` when only the legacy folder exists. Best effort: on failure the
/// old folder stays where it is and the app starts with an empty store.
fn migrate_dir(legacy: &Path, current: &Path) {
    if legacy.is_dir() && !current.exists() {
        let _ = fs::rename(legacy, current);
    }
}

/// A new unique connection id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct ConnectionStore {
    path: PathBuf,
    db: rusqlite::Connection,
    connections: Vec<ConnectionConfig>,
}

impl ConnectionStore {
    /// Opens the store at [`default_path`]. First moves over the folder from the app's old name,
    /// then imports the old `connections.json` if the database doesn't exist yet.
    pub fn open_default() -> Result<Self> {
        let path = default_path().ok_or_else(|| Error::Storage("no home directory".into()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        if let Some(legacy) = config_dir(LEGACY_DIR_NAME) {
            migrate_dir(&legacy, dir);
        }
        let json = dir.join(LEGACY_JSON_FILE);
        if !path.exists() && json.is_file() {
            return Self::import_json_store(&path, &json);
        }
        Self::open(path)
    }

    /// Opens (creating it and its folder if needed) the database at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let err = |e: &dyn std::fmt::Display| storage(&path, e);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| err(&e))?;
        }
        let existed = path.exists();
        let db = rusqlite::Connection::open(&path).map_err(|e| err(&e))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            // Hostnames and usernames are still worth keeping private (-wal/-shm inherit this).
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| err(&e))?;
        }
        db.busy_timeout(Duration::from_secs(5)).map_err(|e| err(&e))?;
        db.pragma_update(None, "journal_mode", "wal").map_err(|e| err(&e))?;
        migrate(&db).map_err(|e| match e {
            Error::Storage(msg) => Error::Storage(format!("{}: {msg}", path.display())),
            other => other,
        })?;
        let mut store = Self { path, db, connections: Vec::new() };
        store.reload()?;
        Ok(store)
    }

    /// Creates the database at `path` from the old JSON store at `json`, then renames the JSON
    /// file to `*.migrated`. On failure nothing is left behind and the JSON file is untouched.
    fn import_json_store(path: &Path, json: &Path) -> Result<Self> {
        let result = (|| {
            let configs = read_json_store(json)?;
            let mut store = Self::open(path)?;
            store.write(|tx| {
                for config in &configs {
                    insert_or_update(tx, config)?;
                }
                Ok(())
            })?;
            Ok(store)
        })();
        match result {
            Ok(store) => {
                let _ = fs::rename(json, json.with_extension("json.migrated"));
                Ok(store)
            }
            Err(e) => {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = fs::remove_file(format!("{}{suffix}", path.display()));
                }
                Err(e)
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Saved connections in user order, as of the last open/write/[`reload`](Self::reload).
    /// Passwords are always `None`.
    pub fn connections(&self) -> &[ConnectionConfig] {
        &self.connections
    }

    pub fn get(&self, id: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Re-reads the list from disk (picks up changes made by another app instance).
    pub fn reload(&mut self) -> Result<()> {
        self.connections = load(&self.db).map_err(|e| storage(&self.path, e))?;
        Ok(())
    }

    /// Adds or replaces (by id) a connection and saves. An empty id gets a fresh one; new
    /// connections go last. Returns the stored config (password stripped).
    pub fn upsert(&mut self, config: ConnectionConfig) -> Result<ConnectionConfig> {
        config.validate()?;
        let mut stored = normalized(&config);
        if stored.id.is_empty() {
            stored.id = new_id();
        }
        self.write(|tx| insert_or_update(tx, &stored))?;
        Ok(stored)
    }

    /// Removes a connection and saves. Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let mut removed = false;
        self.write(|tx| {
            removed = tx.execute("delete from connections where id = ?1", [id])? > 0;
            Ok(())
        })?;
        Ok(removed)
    }

    /// The database last browsed on connection `id` (see [`set_last_database`](Self::set_last_database)).
    pub fn last_database(&self, id: &str) -> Option<String> {
        self.db
            .query_row("select last_database from connections where id = ?1", [id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    /// Remembers the database browsed on connection `id`; `None` means its own `database`.
    /// Cleared automatically when the connection's `database` is edited.
    pub fn set_last_database(&mut self, id: &str, database: Option<&str>) -> Result<()> {
        let database = database.map(str::trim).filter(|d| !d.is_empty());
        self.db
            .execute("update connections set last_database = ?2 where id = ?1", params![id, database])
            .map_err(|e| storage(&self.path, e))?;
        Ok(())
    }

    /// Runs `body` in an immediate transaction, then reloads the list.
    fn write(&mut self, body: impl FnOnce(&Transaction) -> rusqlite::Result<()>) -> Result<()> {
        let path = self.path.clone();
        let err = |e: rusqlite::Error| storage(&path, e);
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(err)?;
        body(&tx).map_err(err)?;
        tx.commit().map_err(err)?;
        self.reload()
    }
}

/// Brings the schema up to date. Refuses files written by a newer dbear.
fn migrate(db: &rusqlite::Connection) -> Result<()> {
    let sql_err = |e: rusqlite::Error| Error::Storage(e.to_string());
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(sql_err)?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > MIGRATIONS.len() {
        return Err(Error::Storage("written by a newer version of dbear".into()));
    }
    for (n, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = db.unchecked_transaction().map_err(sql_err)?;
        tx.execute_batch(sql).map_err(sql_err)?;
        tx.pragma_update(None, "user_version", (n + 1) as i64).map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
    }
    Ok(())
}

/// Trimmed, password-free, with an empty name replaced by the default one.
fn normalized(c: &ConnectionConfig) -> ConnectionConfig {
    let name = c.name.trim();
    ConnectionConfig {
        id: c.id.clone(),
        name: if name.is_empty() { c.default_name() } else { name.into() },
        group: c.group.trim().into(),
        kind: c.kind,
        host: c.host.trim().into(),
        port: c.port,
        database: c.database.trim().into(),
        user: c.user.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(Into::into),
        password: None,
        ssl_mode: c.ssl_mode,
        show_all_databases: c.show_all_databases,
    }
}

/// Serde name of a unit enum variant (`"postgres"`, `"verify-full"`), so the file matches the JSON/URL spelling.
fn enum_text<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(Into::into)).unwrap_or_default()
}

fn parse_enum<T: DeserializeOwned>(column: usize, text: String) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(text)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn load(db: &rusqlite::Connection) -> rusqlite::Result<Vec<ConnectionConfig>> {
    let mut stmt = db.prepare_cached(
        "select id, name, grp, kind, host, port, database, user, ssl_mode, show_all_databases
         from connections order by position, rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ConnectionConfig {
            id: r.get(0)?,
            name: r.get(1)?,
            group: r.get(2)?,
            kind: parse_enum::<DatabaseKind>(3, r.get(3)?)?,
            host: r.get(4)?,
            port: r.get(5)?,
            database: r.get(6)?,
            user: r.get(7)?,
            password: None,
            ssl_mode: parse_enum::<SslMode>(8, r.get(8)?)?,
            show_all_databases: r.get(9)?,
        })
    })?
    .collect();
    rows
}

/// Updates the row in place (keeping its position) or appends it.
fn insert_or_update(tx: &Transaction, c: &ConnectionConfig) -> rusqlite::Result<()> {
    let exists = tx.query_row("select 1 from connections where id = ?1", [&c.id], |_| Ok(())).optional()?.is_some();
    let values = params![
        c.id, c.name, c.group, enum_text(&c.kind), c.host, c.port, c.database, c.user,
        enum_text(&c.ssl_mode), c.show_all_databases,
    ];
    if exists {
        tx.execute(
            "update connections set name = ?2, grp = ?3, kind = ?4, host = ?5, port = ?6,
             last_database = case when database = ?7 and host = ?5 and show_all_databases = ?10
                                  then last_database end,
             database = ?7, user = ?8, ssl_mode = ?9, show_all_databases = ?10 where id = ?1",
            values,
        )?;
    } else {
        tx.execute(
            "insert into connections (id, name, grp, kind, host, port, database, user, ssl_mode, show_all_databases, position)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, (select coalesce(max(position), -1) + 1 from connections))",
            values,
        )?;
    }
    Ok(())
}

// MARK: Legacy JSON store (read once for migration)

/// Shape of a connection in `connections.json` (format version 1).
#[derive(Deserialize)]
struct JsonConnection {
    id: String,
    name: String,
    #[serde(default)]
    group: String,
    kind: DatabaseKind,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    database: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    ssl_mode: SslMode,
    /// Missing in files written before multi-database support: show them all.
    #[serde(default = "default_true")]
    show_all_databases: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct JsonFile {
    version: u32,
    connections: Vec<JsonConnection>,
}

fn read_json_store(path: &Path) -> Result<Vec<ConnectionConfig>> {
    let bytes = fs::read(path).map_err(|e| storage(path, e))?;
    let file: JsonFile = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Storage(format!("{} is not valid: {e}", path.display())))?;
    if file.version > 1 {
        return Err(Error::Storage(format!("{} was written by a newer version of dbear", path.display())));
    }
    Ok(file
        .connections
        .into_iter()
        .map(|s| {
            normalized(&ConnectionConfig {
                id: s.id,
                name: s.name,
                group: s.group,
                kind: s.kind,
                host: s.host,
                port: s.port,
                database: s.database,
                user: s.user,
                password: None,
                ssl_mode: s.ssl_mode,
                show_all_databases: s.show_all_databases,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> ConnectionConfig {
        ConnectionConfig {
            name: name.into(),
            group: "Local".into(),
            database: "app".into(),
            user: Some("postgres".into()),
            password: Some("secret".into()),
            ..ConnectionConfig::new_empty(DatabaseKind::Postgres)
        }
    }

    #[test]
    fn new_database_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dbear.db");
        let store = ConnectionStore::open(&path).unwrap();
        assert!(store.connections().is_empty());
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn saves_and_reloads_without_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        let mut store = ConnectionStore::open(&path).unwrap();
        let saved = store.upsert(ConnectionConfig { ssl_mode: SslMode::VerifyFull, port: Some(6543), ..sample("dev") }).unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(saved.password, None);
        drop(store);

        let reloaded = ConnectionStore::open(&path).unwrap();
        assert_eq!(reloaded.connections(), [saved]);
        let ssl: String = reloaded.db.query_row("select ssl_mode from connections", [], |r| r.get(0)).unwrap();
        assert_eq!(ssl, "verify-full");
        for file in fs::read_dir(dir.path()).unwrap() {
            let bytes = fs::read(file.unwrap().path()).unwrap();
            assert!(!bytes.windows(6).any(|w| w == b"secret"), "password leaked to disk");
        }
    }

    #[test]
    fn show_all_databases_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        let mut store = ConnectionStore::open(&path).unwrap();
        let saved = store.upsert(sample("x")).unwrap();
        assert!(saved.show_all_databases);
        store.upsert(ConnectionConfig { show_all_databases: false, ..saved }).unwrap();
        assert!(!ConnectionStore::open(&path).unwrap().connections()[0].show_all_databases);
    }

    #[test]
    fn empty_name_defaults_to_database_then_host() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path().join("dbear.db")).unwrap();
        let with_db = store.upsert(ConnectionConfig { name: " ".into(), ..sample("x") }).unwrap();
        assert_eq!(with_db.name, "app");
        let no_db = ConnectionConfig { name: String::new(), database: String::new(), host: "db.internal".into(), ..sample("x") };
        assert_eq!(store.upsert(no_db).unwrap().name, "db.internal");
    }

    #[test]
    fn upsert_replaces_in_place_and_remove_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path().join("dbear.db")).unwrap();
        let a = store.upsert(sample("a")).unwrap();
        let b = store.upsert(sample("b")).unwrap();
        store.upsert(ConnectionConfig { name: "a2".into(), ..a.clone() }).unwrap();
        let names: Vec<_> = store.connections().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a2", "b"]);

        assert!(store.remove(&a.id).unwrap());
        assert!(!store.remove(&a.id).unwrap());
        assert_eq!(ConnectionStore::open(store.path()).unwrap().connections(), [b]);
    }

    #[test]
    fn rejects_invalid_config_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path().join("dbear.db")).unwrap();
        let no_host = ConnectionConfig { host: " ".into(), ..sample("x") };
        assert!(matches!(store.upsert(no_host), Err(Error::InvalidConfig(_))));
        assert!(store.connections().is_empty());
    }

    #[test]
    fn two_instances_see_each_others_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        let mut one = ConnectionStore::open(&path).unwrap();
        let mut two = ConnectionStore::open(&path).unwrap();
        let a = one.upsert(sample("a")).unwrap();
        let b = two.upsert(sample("b")).unwrap();
        // `two` saw `a` when it reloaded after its own write; `one` catches up on reload.
        assert_eq!(two.connections(), [a.clone(), b.clone()]);
        one.reload().unwrap();
        assert_eq!(one.connections(), [a, b]);
    }

    #[test]
    fn reports_corrupt_and_future_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        fs::write(&path, "definitely not sqlite, just some text that is long enough").unwrap();
        assert!(matches!(ConnectionStore::open(&path), Err(Error::Storage(_))));

        let future = dir.path().join("future.db");
        rusqlite::Connection::open(&future).unwrap().pragma_update(None, "user_version", 99).unwrap();
        let Err(Error::Storage(msg)) = ConnectionStore::open(&future) else { panic!("expected a storage error") };
        assert!(msg.contains("newer version"), "{msg}");
    }

    #[test]
    fn imports_the_json_store_once() {
        let dir = tempfile::tempdir().unwrap();
        let (db, json) = (dir.path().join("dbear.db"), dir.path().join("connections.json"));
        fs::write(
            &json,
            r#"{"version":1,"connections":[
                {"id":"b","name":"second","kind":"postgres","host":"h","database":"d","ssl_mode":"require"},
                {"id":"a","name":"first","group":"Prod","kind":"mysql","host":"m","port":3307,"database":"","user":"root","show_all_databases":false}
            ]}"#,
        )
        .unwrap();
        let store = ConnectionStore::import_json_store(&db, &json).unwrap();
        let got: Vec<_> = store.connections().iter().map(|c| (c.id.as_str(), c.ssl_mode, c.show_all_databases)).collect();
        assert_eq!(got, [("b", SslMode::Require, true), ("a", SslMode::Prefer, false)]);
        assert_eq!(store.connections()[1].port, Some(3307));
        assert!(!json.exists() && dir.path().join("connections.json.migrated").exists());
    }

    #[test]
    fn failed_json_import_leaves_no_database() {
        let dir = tempfile::tempdir().unwrap();
        let (db, json) = (dir.path().join("dbear.db"), dir.path().join("connections.json"));
        fs::write(&json, "{nope").unwrap();
        assert!(matches!(ConnectionStore::import_json_store(&db, &json), Err(Error::Storage(_))));
        assert!(!db.exists() && json.exists());
        fs::write(&json, r#"{"version": 99, "connections": []}"#).unwrap();
        assert!(ConnectionStore::import_json_store(&db, &json).is_err());
        assert!(!db.exists());
    }

    #[test]
    fn migrates_the_legacy_folder_once() {
        let dir = tempfile::tempdir().unwrap();
        let (legacy, current) = (dir.path().join("DBGui"), dir.path().join("dbear"));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("connections.json"), r#"{"version": 1, "connections": []}"#).unwrap();
        migrate_dir(&legacy, &current);
        assert!(!legacy.exists() && current.join("connections.json").exists());

        // Never overwrites an existing folder.
        fs::create_dir_all(&legacy).unwrap();
        migrate_dir(&legacy, &current);
        assert!(legacy.exists());
    }

    #[test]
    fn default_path_is_platform_specific() {
        let p = default_path().unwrap();
        assert!(p.ends_with(DATABASE_FILE));
        if cfg!(target_os = "macos") {
            assert!(p.to_string_lossy().contains("Library/Application Support/dbear"));
        }
    }

    #[test]
    fn remembers_the_last_database_until_the_database_is_edited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        let mut store = ConnectionStore::open(&path).unwrap();
        let saved = store.upsert(sample("dev")).unwrap();
        assert_eq!(store.last_database(&saved.id), None);

        store.set_last_database(&saved.id, Some("billing")).unwrap();
        assert_eq!(ConnectionStore::open(&path).unwrap().last_database(&saved.id).as_deref(), Some("billing"));

        // Renaming keeps it; pointing the connection at another database forgets it.
        store.upsert(ConnectionConfig { name: "renamed".into(), ..saved.clone() }).unwrap();
        assert_eq!(store.last_database(&saved.id).as_deref(), Some("billing"));
        store.upsert(ConnectionConfig { database: "other".into(), ..saved.clone() }).unwrap();
        assert_eq!(store.last_database(&saved.id), None);

        store.set_last_database(&saved.id, Some("x")).unwrap();
        store.set_last_database(&saved.id, None).unwrap();
        assert_eq!(store.last_database(&saved.id), None);
        assert_eq!(store.last_database("missing"), None);
    }

    #[test]
    fn upgrades_a_version_1_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbear.db");
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(MIGRATIONS[0]).unwrap();
            db.pragma_update(None, "user_version", 1).unwrap();
            db.execute(
                "insert into connections (id, position, name, kind, ssl_mode) values ('a', 0, 'old', 'postgres', 'prefer')",
                [],
            )
            .unwrap();
        }
        let mut store = ConnectionStore::open(&path).unwrap();
        assert_eq!(store.connections().len(), 1);
        store.set_last_database("a", Some("app")).unwrap();
        assert_eq!(store.last_database("a").as_deref(), Some("app"));
    }
}
