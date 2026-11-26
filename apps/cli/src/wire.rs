//! Params and results of the `serve` methods: the server writes them, the one-shot commands read
//! them back (so both sides share one definition). The models inside are dbcore's serde types.

use std::path::PathBuf;

use dbcore::complete::Completions;
use dbcore::dump::{Compression, DataStyle, DumpContent, DumpScope};
use dbcore::edit::{EditStatement, RowChange};
use dbcore::keyset::RowPage;
use dbcore::state::HistoryEntry;
use dbcore::{ConnectionConfig, Error, RowQuery, Schema, TableColumns, TableInfo, TableStructure, Value};
use dbservice::{Problem, ScriptOutcome, TableEditing, Target};
use serde::{Deserialize, Deserializer, Serialize};

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

// MARK: params

/// Which connection (saved id, exact name or URL) and, optionally, which of its server's databases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct On {
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

impl On {
    pub fn new(connection: &str, database: Option<&str>) -> Self {
        Self { connection: connection.to_string(), database: database.map(str::to_string) }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionParams {
    pub connection: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UrlParams {
    pub connection: String,
    #[serde(default)]
    pub with_password: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemasParams {
    #[serde(flatten)]
    pub on: On,
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableParams {
    #[serde(flatten)]
    pub on: On,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
}

pub const DEFAULT_PAGE: u32 = 100;

fn default_page() -> u32 {
    DEFAULT_PAGE
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RowsParams {
    #[serde(flatten)]
    pub table: TableParams,
    #[serde(default)]
    pub query: RowQuery,
    #[serde(default = "default_page")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `table.changes.preview` / `table.changes.apply`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangesParams {
    #[serde(flatten)]
    pub table: TableParams,
    pub changes: Vec<RowChange>,
    /// `apply` only: the SQL the user confirmed; anything else is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<Vec<String>>,
}

/// `table.related`: the rows of `table` whose `columns` hold `values` (no columns: its primary key).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelatedParams {
    #[serde(flatten)]
    pub table: TableParams,
    #[serde(default)]
    pub columns: Vec<String>,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunParams {
    #[serde(flatten)]
    pub on: On,
    pub sql: String,
    /// Absent: 10 000. `null` or 0: every row.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<Option<u32>>,
    #[serde(default)]
    pub record_history: bool,
    #[serde(default)]
    pub describe_sources: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CancelParams {
    /// The `query.run` request's id.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompleteParams {
    #[serde(flatten)]
    pub on: On,
    pub sql: String,
    /// UTF-8 byte offset of the cursor in `sql`.
    pub offset: usize,
}

pub const DEFAULT_HISTORY: usize = 50;

fn default_history() -> usize {
    DEFAULT_HISTORY
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryParams {
    pub connection: String,
    #[serde(default = "default_history")]
    pub limit: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DumpParams {
    #[serde(flatten)]
    pub on: On,
    /// Absolute, or relative to the server's working directory. Absent: `<database>-<date>.sql[.gz]` there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<DumpContent>,
    /// Absent: gzip when the path ends in `.gz`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_style: Option<DataStyle>,
    #[serde(default)]
    pub drop_objects: bool,
    #[serde(default)]
    pub create_database: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DumpScope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreParams {
    #[serde(flatten)]
    pub on: On,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_transaction: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_on_error: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobParams {
    pub job: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisconnectParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
}

// MARK: results

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    pub pid: u32,
    /// `stdio`, `socket` or `in-process` (a one-shot command without a server).
    pub transport: String,
    pub store_path: Option<PathBuf>,
    /// What didn't open (keychain, state file); the server works without them.
    pub problems: Vec<Problem>,
}

/// A saved connection (no secrets) with what a list shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listed {
    #[serde(flatten)]
    pub config: ConnectionConfig,
    /// e.g. `PostgreSQL · localhost:5432/app_dev`.
    pub summary: String,
    pub browses_databases: bool,
}

impl Listed {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { summary: config.summary(), browses_databases: dbservice::browses_databases(&config), config }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionList {
    pub connections: Vec<Listed>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionDetail {
    #[serde(flatten)]
    pub listed: Listed,
    /// Where it opens without a `database` param (the remembered database, else its own).
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlResult {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Databases {
    pub databases: Vec<String>,
    /// The connection's own database.
    pub default: String,
    /// Where it opens without a `database` param.
    pub current: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schemas {
    pub target: Target,
    pub schemas: Vec<Schema>,
}

/// `columns.list`: every table's and view's columns, in one call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnsList {
    pub target: Target,
    pub tables: Vec<TableColumns>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Described {
    pub target: Target,
    pub table: TableInfo,
    pub structure: TableStructure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rows {
    pub target: Target,
    pub table: TableInfo,
    #[serde(flatten)]
    pub page: RowPage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Editable {
    pub target: Target,
    #[serde(flatten)]
    pub editing: TableEditing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesPreview {
    pub target: Target,
    pub table: TableInfo,
    /// In the order they run: deletes, updates, inserts.
    pub statements: Vec<EditStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesApplied {
    pub target: Target,
    pub table: TableInfo,
    pub rows_affected: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Related {
    pub target: Target,
    pub table: TableInfo,
    /// A `filter` for `table.rows`.
    pub filter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub target: Target,
    #[serde(flatten)]
    pub outcome: ScriptOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResult {
    pub cancelled: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    pub target: Target,
    #[serde(flatten)]
    pub completions: Completions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub entries: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStarted {
    pub job: String,
    pub target: Target,
    /// Absolute.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobCancelled {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disconnected {
    /// Connection ids that had sessions open.
    pub connections: Vec<String>,
}

// MARK: notifications

/// `dump.progress` / `restore.progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress<P> {
    pub job: String,
    pub progress: P,
    /// 0…1 when it can tell.
    pub fraction: Option<f64>,
}

/// `{"ok": T}` or `{"error": dbcore::Error}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome<T> {
    Ok(T),
    Error(Error),
}

/// `dump.finished` / `restore.finished`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finished<S> {
    pub job: String,
    pub result: Outcome<S>,
}
