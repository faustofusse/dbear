use async_trait::async_trait;

use crate::access::{DatabaseAccess, DatabaseLevelContext, Grant, Role, RoleRef};

use crate::edit::EditStatement;
use crate::keyset::{PageCursor, RowPage};
use crate::model::{ConnectionConfig, QueryResult, RowQuery, Schema, TableColumns, TableInfo, TableStructure};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Table not found: {0}")]
    TableNotFound(String),
    #[error("Unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Query(String),
    #[error("Query cancelled")]
    Cancelled,
    #[error("{0}")]
    InvalidConfig(String),
    #[error("Couldn’t save connections: {0}")]
    Storage(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Implemented by every database backend (Postgres, MySQL, SQLite, mock).
///
/// Drivers run on the core's own tokio runtime; frontends never call them directly
/// but go through [`crate::Connection`], which is executor-agnostic.
#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn config(&self) -> &ConnectionConfig;
    async fn connect(&self) -> Result<()>;
    /// Closes server connections (cancelling a running `execute`). The next call reconnects.
    async fn disconnect(&self);
    /// Whether a server connection is currently open (and not dropped by the server).
    async fn is_connected(&self) -> bool;
    /// Databases on the same server this login can connect to, sorted. Defaults to just the
    /// configured one (e.g. SQLite files).
    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(vec![self.config().database.clone()])
    }
    async fn list_schemas(&self) -> Result<Vec<Schema>>;
    /// Columns of every table and view this connection can see, for SQL completion.
    /// Grouped by table, never one column at a time.
    async fn list_columns(&self) -> Result<Vec<TableColumns>>;
    /// One page of a table in a stable order: `query.sort`, then the primary key (or row id).
    /// `query.filter` is a `WHERE` expression, already checked by `dialect::normalize_filter`.
    /// `total_count` is only computed for the first page (`offset == 0`), so loading further
    /// pages stays cheap; with a filter it may be `None` on big tables (counting would scan them).
    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult>;
    /// One page of a table after `after` (`None`: the first page), in the same order as `fetch_rows`.
    /// `next` is `None` on the last page; `total_count` is only computed for the first one.
    /// Drivers seek past the last row when they can (see [`crate::keyset`]); this default pages
    /// with OFFSET.
    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let offset = PageCursor::offset_for(after, table, query);
        let result = self.fetch_rows(table, query, limit.saturating_add(1), offset).await?;
        Ok(RowPage::from_offset(result, table, query, limit, offset))
    }
    /// Columns, keys, indexes, foreign keys and DDL of a table or view.
    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure>;
    /// Runs a script. At most `max_rows` rows are kept; the rest are counted and dropped
    /// (`truncated` + `total_count`), so huge results can't exhaust memory.
    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult>;
    /// Cancels the running `execute`, if any. It then fails with [`Error::Cancelled`].
    async fn cancel(&self) {}
    /// Runs edit statements (see [`crate::edit`]) in one transaction on a session of their own:
    /// all or nothing, checked with `edit::check_affected`. Returns the rows affected.
    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let _ = statements;
        Err(Error::Unsupported("this connection is read-only".into()))
    }
    /// Users and roles on the server, system ones included (`Role::is_system`). See [`crate::access`].
    async fn list_roles(&self) -> Result<Vec<Role>> {
        Err(Error::Unsupported(format!("{} has no users to manage here", self.config().kind.display_name())))
    }
    /// Privileges granted directly to `role` (not inherited). Postgres: in the current database.
    async fn list_grants(&self, role: &RoleRef) -> Result<Vec<Grant>> {
        let _ = role;
        Err(Error::Unsupported(format!("{} has no users to manage here", self.config().kind.display_name())))
    }
    /// `role`'s level in `database`, with what's needed to change it. Postgres drivers must be
    /// connected to `database` (see `Connection::database_level`).
    async fn database_level(&self, role: &RoleRef, database: &str) -> Result<DatabaseLevelContext> {
        let _ = (role, database);
        Err(Error::Unsupported(format!("{} has no users to manage here", self.config().kind.display_name())))
    }
    /// `role`'s privileges on every database of the server (also for a role not created yet: none).
    async fn list_database_access(&self, role: &RoleRef) -> Result<Vec<DatabaseAccess>> {
        let _ = role;
        Err(Error::Unsupported(format!("{} has no users to manage here", self.config().kind.display_name())))
    }
}
