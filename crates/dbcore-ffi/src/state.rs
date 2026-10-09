//! What the app remembers between launches besides connections: UI state (open tabs) and each
//! connection's query history (`dbcore::state`, the `state.db` file beside the connection store).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbcore::state as core;

use crate::DbError;

/// One script run, newest first in [`StateStore::history`].
#[derive(uniffi::Record)]
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

impl From<core::HistoryEntry> for HistoryEntry {
    fn from(e: core::HistoryEntry) -> Self {
        Self {
            id: e.id,
            connection_id: e.connection_id,
            database: e.database,
            sql: e.sql,
            ran_at: e.ran_at,
            duration_ms: e.duration_ms,
            rows: e.rows,
            error: e.error,
        }
    }
}

/// A run to record.
#[derive(uniffi::Record)]
pub struct NewHistoryEntry {
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub duration_ms: Option<u64>,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

#[derive(uniffi::Object)]
pub struct StateStore {
    inner: Mutex<core::StateStore>,
}

#[uniffi::export]
impl StateStore {
    /// Opens `state.db` in the folder of the connection store at `store_path`.
    #[uniffi::constructor]
    pub fn open_beside(store_path: String) -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self { inner: Mutex::new(core::StateStore::open_beside(std::path::Path::new(&store_path))?) }))
    }

    /// Opens (creating it if needed) the state file at `path`.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self { inner: Mutex::new(core::StateStore::open(path)?) }))
    }

    pub fn path(&self) -> String {
        self.lock().path().display().to_string()
    }

    /// The value saved under `key` (e.g. the open tabs, as JSON).
    pub fn get(&self, key: String) -> Option<String> {
        self.lock().get(&key)
    }

    /// Saves `value` under `key`; `None` removes it.
    pub fn set(&self, key: String, value: Option<String>) -> Result<(), DbError> {
        Ok(self.lock().set(&key, value.as_deref())?)
    }

    /// Records a run (running the same SQL again moves it to the top).
    pub fn add_history(&self, entry: NewHistoryEntry) -> Result<(), DbError> {
        Ok(self.lock().add_history(core::NewHistoryEntry {
            connection_id: entry.connection_id,
            database: entry.database,
            sql: entry.sql,
            duration: entry.duration_ms.map(Duration::from_millis),
            rows: entry.rows,
            error: entry.error,
        })?)
    }

    /// A connection's newest runs first (any database), at most `limit`.
    pub fn history(&self, connection_id: String, limit: u32) -> Result<Vec<HistoryEntry>, DbError> {
        Ok(self.lock().history(&connection_id, limit as usize)?.into_iter().map(Into::into).collect())
    }

    pub fn clear_history(&self, connection_id: String) -> Result<(), DbError> {
        Ok(self.lock().clear_history(&connection_id)?)
    }
}

impl StateStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, core::StateStore> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
