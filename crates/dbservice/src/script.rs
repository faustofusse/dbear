//! Running scripts the way the apps do: a row cap, history, and "the tables may have changed".

use std::time::{Duration, Instant};

use dbcore::dialect::changes_schema;
use dbcore::results::ResultSources;
use dbcore::state::{HistoryEntry, NewHistoryEntry};
use dbcore::{Connection, Error, QueryResult, Result};
use serde::{Deserialize, Serialize};

use crate::{unblock, Service, Target};

/// The most rows a script returns (the rest are dropped; `QueryResult::truncated` says so).
pub const SCRIPT_ROW_LIMIT: u32 = 10_000;

/// How to run a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunOptions {
    /// Row cap; `None`: everything. Default [`SCRIPT_ROW_LIMIT`].
    pub max_rows: Option<u32>,
    /// Add the run to the connection's history (not when cancelled). Default true.
    pub record_history: bool,
    /// Find the tables the result's columns come from (`ResultSources`: editability, links).
    /// Default false (the apps describe them after showing the rows).
    pub describe_sources: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { max_rows: Some(SCRIPT_ROW_LIMIT), record_history: true, describe_sources: false }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptOutcome {
    /// JSON: `{"ok": QueryResult}` or `{"error": Error}` (a cancelled run is `{"error":{"kind":"cancelled"}}`).
    #[serde(with = "outcome_result")]
    pub result: Result<QueryResult>,
    #[serde(rename = "duration_ms", with = "millis")]
    pub duration: Duration,
    /// The script may have changed the tables (DDL, not cancelled): the target's schemas and
    /// catalog were dropped, and a list showing them should load again.
    pub schema_changed: bool,
    /// With `describe_sources`, for results with origins (`None` when it failed or wasn't asked).
    pub sources: Option<ResultSources>,
    /// Why recording the run in the history failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_error: Option<Error>,
}

impl ScriptOutcome {
    pub fn is_cancelled(&self) -> bool {
        matches!(self.result, Err(Error::Cancelled))
    }
}

/// The history entry for a run, or `None` for a cancelled one: rows are those affected when the
/// result has no columns, else its total count (or the rows returned); failures keep the message.
pub fn history_entry(target: &Target, sql: &str, result: &Result<QueryResult>, took: Duration) -> Option<NewHistoryEntry> {
    let (rows, error) = match result {
        Ok(r) if r.columns.is_empty() => (r.rows_affected, None),
        Ok(r) => (Some(r.total_count.unwrap_or(r.rows.len() as u64)), None),
        Err(Error::Cancelled) => return None,
        Err(e) => (None, Some(e.to_string())),
    };
    Some(NewHistoryEntry {
        connection_id: target.connection_id.clone(),
        database: target.database.clone(),
        sql: sql.to_string(),
        duration: Some(took),
        rows,
        error,
    })
}

impl Service {
    /// Runs `sql` (the whole text; the driver splits statements) on `connection`, which must be a
    /// connection to `target`: the shared one ([`Service::connection`]) or one of the caller's own
    /// ([`Service::new_connection`]). Cancel it with `connection.cancel()`. Records the run in the
    /// target's history, and drops the target's schema caches after DDL.
    pub async fn run_script(&self, target: &Target, connection: &Connection, sql: String, options: RunOptions) -> ScriptOutcome {
        let started = Instant::now();
        let result = connection.execute_limited(sql.clone(), options.max_rows).await;
        let duration = started.elapsed();
        let cancelled = matches!(result, Err(Error::Cancelled));
        let mut history_error = None;
        if options.record_history {
            if let Some(entry) = history_entry(target, &sql, &result, duration) {
                history_error = self.add_history(entry).await.err();
            }
        }
        let schema_changed = !cancelled && changes_schema(&sql);
        if schema_changed {
            self.schema_changed(target);
        }
        let sources = match &result {
            Ok(r) if options.describe_sources && !r.origins.is_empty() => connection.describe_result(r.origins.clone()).await.ok(),
            _ => None,
        };
        ScriptOutcome { result, duration, schema_changed, sources, history_error }
    }

    /// [`run_script`](Self::run_script) on the target's shared connection.
    pub async fn run(&self, target: &Target, sql: String, options: RunOptions) -> Result<ScriptOutcome> {
        let connection = self.connection(target).await?;
        Ok(self.run_script(target, &connection, sql, options).await)
    }

    /// Records a run (on a blocking thread). No state file: nothing to do.
    pub async fn add_history(&self, entry: NewHistoryEntry) -> Result<()> {
        if self.inner.state.is_none() {
            return Ok(());
        }
        let service = self.clone();
        unblock(move || match service.state() {
            Some(mut state) => state.add_history(entry),
            None => Ok(()),
        })
        .await
    }

    /// The connection's newest runs first, across its databases. **Blocking.**
    pub fn history(&self, connection_id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        match self.state() {
            Some(state) => state.history(connection_id, limit),
            None => Ok(Vec::new()),
        }
    }

    /// **Blocking.**
    pub fn clear_history(&self, connection_id: &str) -> Result<()> {
        match self.state() {
            Some(mut state) => state.clear_history(connection_id),
            None => Ok(()),
        }
    }
}

mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

mod outcome_result {
    use dbcore::{Error, QueryResult};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum Wire<T> {
        Ok(T),
        Error(Error),
    }

    pub fn serialize<S: Serializer>(r: &dbcore::Result<QueryResult>, s: S) -> Result<S::Ok, S::Error> {
        match r {
            Ok(v) => Wire::Ok(v),
            Err(e) => Wire::Error(e.clone()),
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<dbcore::Result<QueryResult>, D::Error> {
        Ok(match Wire::<QueryResult>::deserialize(d)? {
            Wire::Ok(v) => Ok(v),
            Wire::Error(e) => Err(e),
        })
    }
}
