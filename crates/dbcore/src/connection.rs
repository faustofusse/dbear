use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::access::{self, AccessChange, AccessStatement, Grant, Role, RoleRef};
use crate::driver::{Driver, Error, Result};
use crate::dialect::normalize_filter;
use crate::edit::{self, EditStatement, RowChange};
use crate::keyset::{PageCursor, RowPage};
use crate::model::{ColumnInfo, ConnectionConfig, QueryResult, RowQuery, Schema, TableColumns, TableInfo, TableStructure};
use crate::libsql::LibsqlDriver;
use crate::mock::{self, MockDriver};
use crate::model::DatabaseKind;
use crate::mysql::MysqlDriver;
use crate::postgres::PostgresDriver;
use crate::sqlite::SqliteDriver;
use crate::sqlserver::SqlServerDriver;

/// The core owns its tokio runtime, so callers can await from any executor
/// (Swift concurrency through FFI, GPUI's executor, or tokio itself).
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dbcore")
            .enable_all()
            .build()
            .expect("failed to start dbcore runtime")
    })
}

/// Aborts the spawned task when dropped, so cancelling the caller's future cancels the work.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|r| r.map_err(|e| Error::Internal(e.to_string())))
    }
}

pub(crate) async fn on_runtime<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    AbortOnDrop(runtime().spawn(future)).await?
}

/// Picks the driver for a connection.
fn make_driver(config: ConnectionConfig) -> Arc<dyn Driver> {
    if mock::is_mock(&config) {
        return Arc::new(MockDriver::new(config));
    }
    match config.kind {
        DatabaseKind::Postgres => Arc::new(PostgresDriver::new(config)),
        DatabaseKind::Mysql => Arc::new(MysqlDriver::new(config)),
        DatabaseKind::Sqlite => Arc::new(SqliteDriver::new(config)),
        DatabaseKind::Libsql => Arc::new(LibsqlDriver::new(config)),
        DatabaseKind::SqlServer => Arc::new(SqlServerDriver::new(config)),
    }
}

/// Entry point for frontends: one per configured connection. Cheap to clone.
#[derive(Clone)]
pub struct Connection {
    driver: Arc<dyn Driver>,
}

impl Connection {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { driver: make_driver(config) }
    }

    pub fn config(&self) -> &ConnectionConfig {
        self.driver.config()
    }

    pub async fn connect(&self) -> Result<()> {
        let d = self.driver.clone();
        on_runtime(async move { d.connect().await }).await
    }

    pub async fn disconnect(&self) {
        let d = self.driver.clone();
        let _ = on_runtime(async move {
            d.disconnect().await;
            Ok(())
        })
        .await;
    }

    pub async fn is_connected(&self) -> bool {
        let d = self.driver.clone();
        on_runtime(async move { Ok(d.is_connected().await) }).await.unwrap_or(false)
    }

    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let d = self.driver.clone();
        on_runtime(async move { d.list_databases().await }).await
    }

    pub async fn list_schemas(&self) -> Result<Vec<Schema>> {
        let d = self.driver.clone();
        on_runtime(async move { d.list_schemas().await }).await
    }

    pub async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        let d = self.driver.clone();
        on_runtime(async move { d.list_columns().await }).await
    }

    /// One page of a table in its natural order (primary key), unfiltered.
    pub async fn fetch_rows(&self, table: TableInfo, limit: u32, offset: u64) -> Result<QueryResult> {
        self.fetch_rows_with(table, RowQuery::default(), limit, offset).await
    }

    /// One page of a table, sorted and filtered by `query` (see [`RowQuery`]).
    pub async fn fetch_rows_with(&self, table: TableInfo, mut query: RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        query.filter = normalize_filter(query.filter.as_deref())?;
        let d = self.driver.clone();
        on_runtime(async move { d.fetch_rows(&table, &query, limit, offset).await }).await
    }

    /// One page of a table after `after` (`None`: the first page); see [`Driver::fetch_page`].
    pub async fn fetch_page(&self, table: TableInfo, mut query: RowQuery, limit: u32, after: Option<PageCursor>) -> Result<RowPage> {
        query.filter = normalize_filter(query.filter.as_deref())?;
        let d = self.driver.clone();
        on_runtime(async move { d.fetch_page(&table, &query, limit, after.as_ref()).await }).await
    }

    /// The SQL that `apply_changes` would run, for review (no connection needed).
    pub fn preview_changes(&self, table: &TableInfo, columns: &[ColumnInfo], changes: &[RowChange]) -> Result<Vec<EditStatement>> {
        edit::statements(self.config().kind, table, columns, changes)
    }

    /// Saves row edits in one transaction: either all of them or none. `columns` are the table's
    /// columns as loaded (they carry the primary key). Returns the number of rows affected.
    pub async fn apply_changes(&self, table: TableInfo, columns: Vec<ColumnInfo>, changes: Vec<RowChange>) -> Result<u64> {
        let statements = self.preview_changes(&table, &columns, &changes)?;
        if statements.is_empty() {
            return Ok(0);
        }
        let d = self.driver.clone();
        on_runtime(async move { d.apply(&statements).await }).await
    }

    pub async fn describe_table(&self, table: TableInfo) -> Result<TableStructure> {
        let d = self.driver.clone();
        on_runtime(async move { d.describe_table(&table).await }).await
    }

    /// Runs a script and keeps every row. Dropping the returned future also cancels the query on the server.
    pub async fn execute(&self, sql: String) -> Result<QueryResult> {
        self.execute_limited(sql, None).await
    }

    /// Like [`Connection::execute`], keeping at most `max_rows` rows (see [`QueryResult::truncated`]).
    pub async fn execute_limited(&self, sql: String, max_rows: Option<u32>) -> Result<QueryResult> {
        let d = self.driver.clone();
        on_runtime(async move { d.execute(&sql, max_rows).await }).await
    }

    /// Users and roles on the server (see [`crate::access`]).
    pub async fn list_roles(&self) -> Result<Vec<Role>> {
        let d = self.driver.clone();
        on_runtime(async move { d.list_roles().await }).await
    }

    /// Privileges granted directly to `role` (Postgres: in this connection's database).
    pub async fn list_grants(&self, role: RoleRef) -> Result<Vec<Grant>> {
        let d = self.driver.clone();
        on_runtime(async move { d.list_grants(&role).await }).await
    }

    /// The statements `apply_access` would run (passwords masked in `display`).
    pub fn preview_access(&self, change: &AccessChange) -> Result<Vec<AccessStatement>> {
        access::statements(self.config().kind, change)
    }

    /// Creates, changes or drops a role, or changes its privileges. Runs in one transaction where the
    /// database allows it (Postgres); MySQL commits each account statement as it goes.
    pub async fn apply_access(&self, change: AccessChange) -> Result<()> {
        let kind = self.config().kind;
        let statements: Vec<EditStatement> = self
            .preview_access(&change)?
            .into_iter()
            .map(|s| EditStatement { sql: s.sql, expect_one_row: false, target: s.display })
            .collect();
        if statements.is_empty() {
            return Ok(());
        }
        let d = self.driver.clone();
        on_runtime(async move { d.apply(&statements).await }).await.map(|_| ()).map_err(|e| match e {
            // Account statements commit implicitly in MySQL: earlier ones stay applied.
            Error::Query(m) if kind == DatabaseKind::Mysql => Error::Query(m.replace(
                "Nothing was saved.",
                "Statements before it were applied (MySQL commits account changes immediately).",
            )),
            other => other,
        })
    }

    /// Cancels the running [`Connection::execute`], which then fails with [`Error::Cancelled`].
    pub async fn cancel(&self) {
        let d = self.driver.clone();
        let _ = on_runtime(async move {
            d.cancel().await;
            Ok(())
        })
        .await;
    }
}
