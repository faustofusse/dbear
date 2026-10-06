//! What frontends remember between launches that isn't a connection: open tabs and other UI state
//! (as JSON under a key) and each connection's query history.
//!
//! It lives in its own SQLite file, `state.db`, next to the connection store. Keeping it out of
//! `dbear.db` means adding it didn't bump that file's schema version, which released apps refuse to
//! open when it's newer than they know. The schema here is versioned the same way
//! (`PRAGMA user_version`), and losing this file only loses history and open tabs.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::driver::{Error, Result};

/// File name, in the same folder as the connection store.
pub const STATE_FILE: &str = "state.db";

/// History kept per connection; older entries are dropped as new ones come in.
pub const HISTORY_LIMIT: usize = 1000;

const MIGRATIONS: &[&str] = &[
    // 1: UI state and query history.
    "create table app_state (
        key   text primary key,
        value text not null
    ) strict;
    create table query_history (
        id            integer primary key,
        connection_id text not null,
        database      text not null default '',
        sql           text not null,
        ran_at        integer not null,
        duration_ms   integer,
        rows          integer,
        error         text
    ) strict;
    create index query_history_recent on query_history (connection_id, ran_at desc, id desc);",
];

/// One script run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub connection_id: String,
    /// The database it ran in (empty: the connection's own).
    pub database: String,
    pub sql: String,
    /// Milliseconds since the Unix epoch.
    pub ran_at: i64,
    pub duration_ms: Option<u64>,
    /// Rows returned, or affected for statements that return none.
    pub rows: Option<u64>,
    /// Why it failed (`None`: it succeeded).
    pub error: Option<String>,
}

/// What to record for a run (see [`StateStore::add_history`]).
#[derive(Debug, Clone, Default)]
pub struct NewHistoryEntry {
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub duration: Option<Duration>,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

pub struct StateStore {
    path: PathBuf,
    db: rusqlite::Connection,
}

fn storage(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Storage(format!("{}: {e}", path.display()))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl StateStore {
    /// Opens `state.db` in the same folder as the connection store at `store_path`.
    pub fn open_beside(store_path: &Path) -> Result<Self> {
        Self::open(store_path.with_file_name(STATE_FILE))
    }

    /// Opens (creating it and its folder if needed) the state file at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| storage(&path, e))?;
        }
        #[cfg(unix)]
        let existed = path.exists();
        let db = rusqlite::Connection::open(&path).map_err(|e| storage(&path, e))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            // Queries can hold sensitive values; keep the file private like the connection store.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| storage(&path, e))?;
        }
        db.busy_timeout(Duration::from_secs(5)).map_err(|e| storage(&path, e))?;
        db.pragma_update(None, "journal_mode", "wal").map_err(|e| storage(&path, e))?;
        migrate(&db).map_err(|e| storage(&path, e))?;
        Ok(Self { path, db })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // MARK: UI state

    /// The value saved under `key`, e.g. a frontend's open tabs as JSON.
    pub fn get(&self, key: &str) -> Option<String> {
        self.db.query_row("select value from app_state where key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    /// Saves `value` under `key`; `None` removes it.
    pub fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let result = match value {
            Some(value) => self.db.execute(
                "insert into app_state (key, value) values (?1, ?2) on conflict (key) do update set value = excluded.value",
                params![key, value],
            ),
            None => self.db.execute("delete from app_state where key = ?1", [key]),
        };
        result.map(drop).map_err(|e| storage(&self.path, e))
    }

    // MARK: history

    /// Records a run. Running the same SQL again in the same place just moves it to the top.
    /// Keeps the newest [`HISTORY_LIMIT`] runs per connection.
    pub fn add_history(&mut self, entry: NewHistoryEntry) -> Result<()> {
        let sql = entry.sql.trim();
        if sql.is_empty() {
            return Ok(());
        }
        let path = self.path.clone();
        let err = |e: rusqlite::Error| storage(&path, e);
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(err)?;
        tx.execute(
            "delete from query_history where connection_id = ?1 and database = ?2 and sql = ?3",
            params![entry.connection_id, entry.database, sql],
        )
        .map_err(err)?;
        tx.execute(
            "insert into query_history (connection_id, database, sql, ran_at, duration_ms, rows, error)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.connection_id,
                entry.database,
                sql,
                now_ms(),
                entry.duration.map(|d| d.as_millis() as i64),
                entry.rows.map(|r| r as i64),
                entry.error,
            ],
        )
        .map_err(err)?;
        tx.execute(
            "delete from query_history where connection_id = ?1 and id not in (
                select id from query_history where connection_id = ?1 order by ran_at desc, id desc limit ?2)",
            params![entry.connection_id, HISTORY_LIMIT as i64],
        )
        .map_err(err)?;
        tx.commit().map_err(err)
    }

    /// The newest runs on `connection_id` first (any database), at most `limit`.
    pub fn history(&self, connection_id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let err = |e: rusqlite::Error| storage(&self.path, e);
        let mut stmt = self
            .db
            .prepare(
                "select id, connection_id, database, sql, ran_at, duration_ms, rows, error from query_history
                 where connection_id = ?1 order by ran_at desc, id desc limit ?2",
            )
            .map_err(err)?;
        let rows = stmt
            .query_map(params![connection_id, limit as i64], |r| {
                Ok(HistoryEntry {
                    id: r.get(0)?,
                    connection_id: r.get(1)?,
                    database: r.get(2)?,
                    sql: r.get(3)?,
                    ran_at: r.get(4)?,
                    duration_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    rows: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    error: r.get(7)?,
                })
            })
            .map_err(err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)
    }

    /// Forgets a connection's history (when the connection is deleted, or on request).
    pub fn clear_history(&mut self, connection_id: &str) -> Result<()> {
        self.db
            .execute("delete from query_history where connection_id = ?1", [connection_id])
            .map(drop)
            .map_err(|e| storage(&self.path, e))
    }
}

fn migrate(db: &rusqlite::Connection) -> std::result::Result<(), String> {
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(|e| e.to_string())?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > MIGRATIONS.len() {
        return Err("written by a newer version of dbear".into());
    }
    for (n, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute_batch(sql).map_err(|e| e.to_string())?;
        tx.pragma_update(None, "user_version", (n + 1) as i64).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open_beside(&dir.path().join("dbear.db")).unwrap();
        (dir, store)
    }

    fn run(sql: &str) -> NewHistoryEntry {
        NewHistoryEntry { connection_id: "c".into(), database: "app".into(), sql: sql.into(), ..Default::default() }
    }

    #[test]
    fn lives_beside_the_connection_store() {
        let (dir, store) = store();
        assert_eq!(store.path(), dir.path().join(STATE_FILE));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn saves_and_removes_values() {
        let (_dir, mut store) = store();
        assert_eq!(store.get("tabs"), None);
        store.set("tabs", Some("[1]")).unwrap();
        store.set("tabs", Some("[1,2]")).unwrap();
        assert_eq!(store.get("tabs").as_deref(), Some("[1,2]"));
        store.set("tabs", None).unwrap();
        assert_eq!(store.get("tabs"), None);
    }

    #[test]
    fn history_is_newest_first_and_moves_repeats_to_the_top() {
        let (_dir, mut store) = store();
        store.add_history(run("select 1")).unwrap();
        store.add_history(NewHistoryEntry { rows: Some(3), duration: Some(Duration::from_millis(12)), ..run("select 2") }).unwrap();
        store.add_history(run("  select 1  ")).unwrap();
        store.add_history(run("   ")).unwrap();
        let history = store.history("c", 10).unwrap();
        assert_eq!(history.iter().map(|h| h.sql.as_str()).collect::<Vec<_>>(), ["select 1", "select 2"]);
        assert_eq!((history[1].rows, history[1].duration_ms), (Some(3), Some(12)));
        assert!(store.history("other", 10).unwrap().is_empty());
        store.clear_history("c").unwrap();
        assert!(store.history("c", 10).unwrap().is_empty());
    }

    #[test]
    fn history_keeps_the_newest_runs() {
        let (_dir, mut store) = store();
        for i in 0..HISTORY_LIMIT + 5 {
            store.add_history(run(&format!("select {i}"))).unwrap();
        }
        let history = store.history("c", HISTORY_LIMIT + 10).unwrap();
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].sql, format!("select {}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn refuses_a_newer_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        rusqlite::Connection::open(&path).unwrap().pragma_update(None, "user_version", 99).unwrap();
        assert!(StateStore::open(&path).is_err());
    }
}
