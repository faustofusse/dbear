//! UniFFI surface of `dbcore` for the SwiftUI app.
//!
//! Types are mirrored here (instead of deriving UniFFI traits in `dbcore`) so the core
//! stays free of FFI concerns; the GPUI app links `dbcore` directly and never sees this crate.
//! Keep this surface small: a few objects plus plain records, with rows sent in pages.

use std::sync::{Arc, Mutex};

uniffi::setup_scaffolding!();

mod access;
mod dump;
mod import;
mod results;
mod state;

// MARK: Records & enums

#[derive(uniffi::Enum, Clone, Copy)]
pub enum DatabaseKind {
    Postgres,
    Mysql,
    Sqlite,
    /// Turso / libSQL (shown as "Turso").
    Libsql,
    SqlServer,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyFull,
}

#[derive(uniffi::Record, Clone)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub group: String,
    pub kind: DatabaseKind,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: SslMode,
    pub show_all_databases: bool,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum TableKind {
    Table,
    View,
}

#[derive(uniffi::Record, Clone)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub estimated_row_count: Option<u64>,
}

#[derive(uniffi::Record)]
pub struct Schema {
    pub name: String,
    pub tables: Vec<TableInfo>,
}

#[derive(uniffi::Record, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub is_primary_key: bool,
    pub is_nullable: bool,
}

#[derive(uniffi::Record, Clone)]
pub struct TableColumns {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

#[derive(uniffi::Enum, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(String),
    Text(String),
}

#[derive(uniffi::Record, Clone)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

/// Sort and `WHERE` filter for browsing a table (see `dbcore::RowQuery`).
#[derive(uniffi::Record, Clone, Default)]
pub struct RowQuery {
    pub sort: Vec<SortKey>,
    pub filter: Option<String>,
}

#[derive(uniffi::Record)]
pub struct ColumnDetail {
    pub name: String,
    pub type_name: String,
    pub is_nullable: bool,
    pub default_value: Option<String>,
    pub is_primary_key: bool,
    pub comment: Option<String>,
}

#[derive(uniffi::Record)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    pub definition: Option<String>,
}

#[derive(uniffi::Record)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

/// A foreign key in another table that points at this one (see `dbcore::ReferencingKey`).
#[derive(uniffi::Record)]
pub struct ReferencingKey {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_columns: Vec<String>,
}

#[derive(uniffi::Record)]
pub struct TableStructure {
    pub columns: Vec<ColumnDetail>,
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub referenced_by: Vec<ReferencingKey>,
    pub ddl: Option<String>,
}

/// A new cell value (see `dbcore::edit`).
#[derive(uniffi::Enum, Clone)]
pub enum EditValue {
    Null,
    Default,
    Text { text: String },
}

#[derive(uniffi::Record, Clone)]
pub struct CellEdit {
    pub column: String,
    pub value: EditValue,
}

#[derive(uniffi::Record, Clone)]
pub struct KeyValue {
    pub column: String,
    pub value: Value,
}

#[derive(uniffi::Enum, Clone)]
pub enum RowChange {
    Update { key: Vec<KeyValue>, set: Vec<CellEdit> },
    Insert { values: Vec<CellEdit> },
    Delete { key: Vec<KeyValue> },
}

#[derive(uniffi::Record)]
pub struct EditStatement {
    pub sql: String,
    pub expect_one_row: bool,
    pub target: String,
}

#[derive(uniffi::Record)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    pub total_count: Option<u64>,
    pub rows_affected: Option<u64>,
    pub truncated: bool,
    /// Script results: the table column each column reads (`None`: an expression). Empty when unknown.
    pub origins: Vec<Option<results::ColumnOrigin>>,
}

/// One page of a table and an opaque token for the next one (`None`: last page).
#[derive(uniffi::Record)]
pub struct RowPage {
    pub result: QueryResult,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum DbError {
    #[error("Connection failed: {message}")]
    ConnectionFailed { message: String },
    #[error("Table not found: {name}")]
    TableNotFound { name: String },
    #[error("Unsupported: {message}")]
    Unsupported { message: String },
    #[error("{message}")]
    Query { message: String },
    #[error("Query cancelled")]
    Cancelled,
    #[error("{message}")]
    InvalidConfig { message: String },
    #[error("Couldn’t save connections: {message}")]
    Storage { message: String },
    #[error("Internal error: {message}")]
    Internal { message: String },
}

// MARK: Objects & functions

#[derive(uniffi::Object)]
pub struct Connection {
    inner: dbcore::Connection,
}

#[uniffi::export]
impl Connection {
    #[uniffi::constructor]
    pub fn new(config: ConnectionConfig) -> Arc<Self> {
        Arc::new(Self { inner: dbcore::Connection::new(config.into()) })
    }

    pub fn config(&self) -> ConnectionConfig {
        self.inner.config().clone().into()
    }

    pub async fn connect(&self) -> Result<(), DbError> {
        Ok(self.inner.connect().await?)
    }

    pub async fn disconnect(&self) {
        self.inner.disconnect().await
    }

    pub async fn is_connected(&self) -> bool {
        self.inner.is_connected().await
    }

    pub async fn list_databases(&self) -> Result<Vec<String>, DbError> {
        Ok(self.inner.list_databases().await?)
    }

    pub async fn list_schemas(&self) -> Result<Vec<Schema>, DbError> {
        Ok(self.inner.list_schemas().await?.into_iter().map(Into::into).collect())
    }

    /// Columns of every table and view, for SQL completion (see `CompletionCatalog`).
    pub async fn list_columns(&self) -> Result<Vec<TableColumns>, DbError> {
        Ok(self.inner.list_columns().await?.into_iter().map(Into::into).collect())
    }

    /// One page of a table, sorted and filtered by `query`.
    pub async fn fetch_rows(&self, table: TableInfo, query: RowQuery, limit: u32, offset: u64) -> Result<QueryResult, DbError> {
        Ok(self.inner.fetch_rows_with(table.into(), query.into(), limit, offset).await?.into())
    }

    /// One page of a table after `after` (a `next_cursor` from the previous page; `None`: the
    /// first page). Seeks past the last row where possible, so deep pages stay fast.
    pub async fn fetch_page(&self, table: TableInfo, query: RowQuery, limit: u32, after: Option<String>) -> Result<RowPage, DbError> {
        let after = after.as_deref().map(dbcore::PageCursor::decode).transpose()?;
        let page = self.inner.fetch_page(table.into(), query.into(), limit, after).await?;
        Ok(RowPage { result: page.result.into(), next_cursor: page.next.map(|c| c.encode()) })
    }

    /// The statements `apply_changes` would run, in order (for the review sheet).
    pub fn preview_changes(&self, table: TableInfo, columns: Vec<ColumnInfo>, changes: Vec<RowChange>) -> Result<Vec<EditStatement>, DbError> {
        let columns: Vec<dbcore::ColumnInfo> = columns.into_iter().map(Into::into).collect();
        let changes: Vec<dbcore::edit::RowChange> = changes.into_iter().map(Into::into).collect();
        Ok(self.inner.preview_changes(&table.into(), &columns, &changes)?.into_iter().map(Into::into).collect())
    }

    /// Saves row edits in one transaction (all or nothing). Returns the rows affected.
    pub async fn apply_changes(&self, table: TableInfo, columns: Vec<ColumnInfo>, changes: Vec<RowChange>) -> Result<u64, DbError> {
        let columns = columns.into_iter().map(Into::into).collect();
        let changes = changes.into_iter().map(Into::into).collect();
        Ok(self.inner.apply_changes(table.into(), columns, changes).await?)
    }

    /// Columns, keys, indexes, foreign keys and DDL of a table or view.
    pub async fn describe_table(&self, table: TableInfo) -> Result<TableStructure, DbError> {
        Ok(self.inner.describe_table(table.into()).await?.into())
    }

    /// Runs a script, keeping at most `max_rows` rows (`None` = all).
    pub async fn execute(&self, sql: String, max_rows: Option<u32>) -> Result<QueryResult, DbError> {
        Ok(self.inner.execute_limited(sql, max_rows).await?.into())
    }

    /// The statement `create_database` runs (validates the name).
    pub fn preview_create_database(&self, name: String) -> Result<String, DbError> {
        Ok(self.inner.preview_create_database(&name)?)
    }

    /// Creates a database on this connection's server.
    pub async fn create_database(&self, name: String) -> Result<(), DbError> {
        Ok(self.inner.create_database(name).await?)
    }

    /// Cancels the running `execute`, which then fails with `DbError::Cancelled`.
    /// (Swift task cancellation doesn't reach Rust futures through UniFFI, so call this.)
    pub async fn cancel(&self) {
        self.inner.cancel().await
    }
}

/// Saved connections (SQLite file, no passwords). Passwords live in the Keychain, owned by the app.
#[derive(uniffi::Object)]
pub struct ConnectionStore {
    inner: Mutex<dbcore::ConnectionStore>,
}

#[uniffi::export]
impl ConnectionStore {
    /// Opens the store at `path`, creating an empty one if it doesn't exist.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self { inner: Mutex::new(dbcore::ConnectionStore::open(path)?) }))
    }

    /// Opens the store at the platform default location (migrating the old DBGui folder and `connections.json`).
    #[uniffi::constructor]
    pub fn open_default() -> Result<Arc<Self>, DbError> {
        Ok(Arc::new(Self { inner: Mutex::new(dbcore::ConnectionStore::open_default()?) }))
    }

    pub fn path(&self) -> String {
        self.lock().path().display().to_string()
    }

    pub fn connections(&self) -> Vec<ConnectionConfig> {
        self.lock().connections().iter().cloned().map(Into::into).collect()
    }

    /// Adds or replaces (by id) and saves. An empty id gets a new one. Returns the stored config.
    pub fn upsert(&self, config: ConnectionConfig) -> Result<ConnectionConfig, DbError> {
        Ok(self.lock().upsert(config.into())?.into())
    }

    pub fn remove(&self, id: String) -> Result<bool, DbError> {
        Ok(self.lock().remove(&id)?)
    }

    /// The database last browsed on connection `id`, if any.
    pub fn last_database(&self, id: String) -> Option<String> {
        self.lock().last_database(&id)
    }

    /// Remembers the database browsed on connection `id` (`None` = its own database).
    pub fn set_last_database(&self, id: String, database: Option<String>) -> Result<(), DbError> {
        Ok(self.lock().set_last_database(&id, database.as_deref())?)
    }
}

impl ConnectionStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, dbcore::ConnectionStore> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Blank config for the "Add Connection" form.
#[uniffi::export]
pub fn new_connection_config(kind: DatabaseKind) -> ConnectionConfig {
    dbcore::ConnectionConfig::new_empty(kind.into()).into()
}

/// First problem with the config, or `None` if it can be saved.
#[uniffi::export]
pub fn validate_connection(config: ConnectionConfig) -> Option<String> {
    dbcore::ConnectionConfig::from(config).validate().err().map(|e| e.to_string())
}

/// Parses `postgres://user:pass@host:port/db?sslmode=…` (and mysql/sqlserver/sqlite URLs).
#[uniffi::export]
pub fn parse_connection_url(url: String) -> Result<ConnectionConfig, DbError> {
    Ok(dbcore::ConnectionConfig::from_url(&url)?.into())
}

#[uniffi::export]
pub fn connection_url(config: ConnectionConfig, include_password: bool) -> String {
    dbcore::ConnectionConfig::from(config).to_url(include_password)
}

#[uniffi::export]
pub fn default_port(kind: DatabaseKind) -> Option<u16> {
    dbcore::DatabaseKind::from(kind).default_port()
}

/// Sample connections (mock data + the dev database) for development.
#[uniffi::export]
pub fn sample_connections() -> Vec<ConnectionConfig> {
    dbcore::mock::connections().into_iter().map(Into::into).collect()
}

/// e.g. "PostgreSQL · localhost:5432/app_dev"
/// The database opened when `database` is empty (Postgres: `postgres`).
#[uniffi::export]
pub fn default_database(config: ConnectionConfig) -> String {
    dbcore::ConnectionConfig::from(config).default_database().to_string()
}

/// Name used when the user leaves it empty: the database, else the host.
#[uniffi::export]
pub fn default_connection_name(config: ConnectionConfig) -> String {
    dbcore::ConnectionConfig::from(config).default_name()
}

#[uniffi::export]
pub fn connection_summary(config: ConnectionConfig) -> String {
    dbcore::ConnectionConfig::from(config).summary()
}

/// Binary columns show a hex preview in the grid, so their cells can't be edited.
#[uniffi::export]
pub fn is_binary_column(column: ColumnInfo) -> bool {
    dbcore::edit::is_binary(&column.into())
}

/// A `WHERE` filter for the rows whose `columns` hold `values` (e.g. the row a foreign key points at).
#[uniffi::export]
pub fn match_filter(kind: DatabaseKind, columns: Vec<String>, values: Vec<Value>) -> String {
    let values: Vec<dbcore::Value> = values.into_iter().map(Into::into).collect();
    dbcore::dialect::Dialect(kind.into()).match_filter(&columns, &values)
}

// MARK: Copying rows

#[derive(uniffi::Enum, Clone, Copy)]
pub enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Markdown,
    Insert,
}

impl From<CopyFormat> for dbcore::export::CopyFormat {
    fn from(f: CopyFormat) -> Self {
        match f {
            CopyFormat::Tsv => Self::Tsv,
            CopyFormat::Csv => Self::Csv,
            CopyFormat::Json => Self::Json,
            CopyFormat::Markdown => Self::Markdown,
            CopyFormat::Insert => Self::Insert,
        }
    }
}

/// Rows as clipboard text. `schema`/`table` name the `INSERT` target (`None` for script results).
#[uniffi::export]
pub fn format_rows(
    format: CopyFormat, kind: DatabaseKind, schema: Option<String>, table: Option<String>, columns: Vec<ColumnInfo>,
    rows: Vec<Vec<Value>>, headers: bool,
) -> String {
    let columns: Vec<dbcore::ColumnInfo> = columns.into_iter().map(Into::into).collect();
    let rows: Vec<Vec<dbcore::Value>> = rows.into_iter().map(|r| r.into_iter().map(Into::into).collect()).collect();
    let target = dbcore::export::Target { kind: kind.into(), schema: schema.as_deref(), table: table.as_deref() };
    dbcore::export::format_rows(format.into(), target, &columns, &rows, headers)
}

/// A JSON object or array re-indented for reading (key order and digits kept), else `None`.
#[uniffi::export]
pub fn pretty_json(text: String) -> Option<String> {
    dbcore::export::pretty_json(&text)
}

#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").into()
}

// MARK: Syntax highlighting

#[derive(uniffi::Enum, Clone, Copy)]
pub enum HighlightKind {
    Keyword,
    Type,
    Object,
    Function,
    Field,
    Variable,
    Parameter,
    String,
    Number,
    Constant,
    Comment,
    Operator,
    Punctuation,
}

/// `location`/`length` are UTF-16 code units, ready for `NSRange`.
#[derive(uniffi::Record)]
pub struct HighlightSpan {
    pub location: u32,
    pub length: u32,
    pub kind: HighlightKind,
}

/// Highlight spans for a SQL script, sorted by location (later spans are more specific).
#[uniffi::export]
pub fn highlight_sql(text: String) -> Vec<HighlightSpan> {
    utf16_spans(&text, dbcore::highlight::highlight_sql(&text))
}

/// Highlight spans for a JSON value (keys are `Field`), sorted by location.
#[uniffi::export]
pub fn highlight_json(text: String) -> Vec<HighlightSpan> {
    utf16_spans(&text, dbcore::highlight::highlight_json(&text))
}

fn utf16_spans(text: &str, spans: Vec<dbcore::highlight::HighlightSpan>) -> Vec<HighlightSpan> {
    if spans.is_empty() {
        return Vec::new();
    }
    // Byte offset -> UTF-16 offset, for every char boundary (plus the end).
    let mut utf16 = vec![0u32; text.len() + 1];
    let mut units = 0u32;
    for (i, c) in text.char_indices() {
        utf16[i] = units;
        units += c.len_utf16() as u32;
    }
    utf16[text.len()] = units;
    spans
        .into_iter()
        .map(|s| HighlightSpan { location: utf16[s.start], length: utf16[s.end] - utf16[s.start], kind: s.kind.into() })
        .collect()
}

impl From<dbcore::highlight::HighlightKind> for HighlightKind {
    fn from(k: dbcore::highlight::HighlightKind) -> Self {
        use dbcore::highlight::HighlightKind as K;
        match k {
            K::Keyword => Self::Keyword,
            K::Type => Self::Type,
            K::Object => Self::Object,
            K::Function => Self::Function,
            K::Field => Self::Field,
            K::Variable => Self::Variable,
            K::Parameter => Self::Parameter,
            K::String => Self::String,
            K::Number => Self::Number,
            K::Constant => Self::Constant,
            K::Comment => Self::Comment,
            K::Operator => Self::Operator,
            K::Punctuation => Self::Punctuation,
        }
    }
}

// MARK: SQL completion

/// Schemas, tables/views and columns a connection can see, built once per database and reused
/// on every keystroke (`list_schemas` + `list_columns`).
#[derive(uniffi::Object)]
pub struct CompletionCatalog {
    inner: dbcore::complete::Catalog,
}

#[uniffi::export]
impl CompletionCatalog {
    #[uniffi::constructor]
    pub fn new(schemas: Vec<Schema>, columns: Vec<TableColumns>) -> Arc<Self> {
        let schemas = schemas.into_iter().map(Into::into).collect();
        let columns = columns.into_iter().map(Into::into).collect();
        Arc::new(Self { inner: dbcore::complete::Catalog::new(schemas, columns) })
    }

    /// Completions for `text` with the caret at `location` (UTF-16 code units, like `NSRange`).
    pub fn complete(&self, text: String, location: u32, kind: DatabaseKind) -> Completions {
        let offset = byte_offset_for_utf16(&text, location);
        let dialect = dbcore::dialect::Dialect(kind.into());
        let result = dbcore::complete::complete(&text, offset, &self.inner, dialect);
        Self::utf16(&text, result)
    }

    /// Completions inside `schema.table`'s `WHERE` filter (`text` is just the condition).
    pub fn complete_filter(&self, text: String, location: u32, kind: DatabaseKind, schema: String, table: String) -> Completions {
        let offset = byte_offset_for_utf16(&text, location);
        let dialect = dbcore::dialect::Dialect(kind.into());
        let result = dbcore::complete::complete_filter(&text, offset, &self.inner, dialect, &schema, &table);
        Self::utf16(&text, result)
    }
}

impl CompletionCatalog {
    fn utf16(text: &str, result: dbcore::complete::Completions) -> Completions {
        let start = utf16_offset_for_byte(text, result.replace_start);
        let end = utf16_offset_for_byte(text, result.replace_end);
        Completions { location: start, length: end - start, items: result.items.into_iter().map(Into::into).collect() }
    }
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum CompletionKind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
    Function,
}

#[derive(uniffi::Record)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

/// `location`/`length` are UTF-16 code units (like `HighlightSpan`): the range of `text` to
/// replace with an item's `insert_text`.
#[derive(uniffi::Record)]
pub struct Completions {
    pub location: u32,
    pub length: u32,
    pub items: Vec<CompletionItem>,
}

fn byte_offset_for_utf16(text: &str, utf16_offset: u32) -> usize {
    let mut units = 0u32;
    for (byte_idx, c) in text.char_indices() {
        if units >= utf16_offset {
            return byte_idx;
        }
        units += c.len_utf16() as u32;
    }
    text.len()
}

fn utf16_offset_for_byte(text: &str, byte_offset: usize) -> u32 {
    let mut units = 0u32;
    for (byte_idx, c) in text.char_indices() {
        if byte_idx >= byte_offset {
            return units;
        }
        units += c.len_utf16() as u32;
    }
    units
}

impl From<dbcore::complete::CompletionKind> for CompletionKind {
    fn from(k: dbcore::complete::CompletionKind) -> Self {
        use dbcore::complete::CompletionKind as K;
        match k {
            K::Keyword => Self::Keyword,
            K::Schema => Self::Schema,
            K::Table => Self::Table,
            K::View => Self::View,
            K::Column => Self::Column,
            K::Function => Self::Function,
        }
    }
}

impl From<dbcore::complete::CompletionItem> for CompletionItem {
    fn from(i: dbcore::complete::CompletionItem) -> Self {
        Self { label: i.label, insert_text: i.insert_text, kind: i.kind.into(), detail: i.detail }
    }
}

// MARK: Conversions

impl From<DatabaseKind> for dbcore::DatabaseKind {
    fn from(k: DatabaseKind) -> Self {
        match k {
            DatabaseKind::Postgres => Self::Postgres,
            DatabaseKind::Mysql => Self::Mysql,
            DatabaseKind::Sqlite => Self::Sqlite,
            DatabaseKind::Libsql => Self::Libsql,
            DatabaseKind::SqlServer => Self::SqlServer,
        }
    }
}

impl From<dbcore::DatabaseKind> for DatabaseKind {
    fn from(k: dbcore::DatabaseKind) -> Self {
        match k {
            dbcore::DatabaseKind::Postgres => Self::Postgres,
            dbcore::DatabaseKind::Mysql => Self::Mysql,
            dbcore::DatabaseKind::Sqlite => Self::Sqlite,
            dbcore::DatabaseKind::Libsql => Self::Libsql,
            dbcore::DatabaseKind::SqlServer => Self::SqlServer,
        }
    }
}

impl From<ConnectionConfig> for dbcore::ConnectionConfig {
    fn from(c: ConnectionConfig) -> Self {
        Self {
            id: c.id,
            name: c.name,
            group: c.group,
            kind: c.kind.into(),
            host: c.host,
            port: c.port,
            database: c.database,
            user: c.user,
            password: c.password,
            ssl_mode: c.ssl_mode.into(),
            show_all_databases: c.show_all_databases,
        }
    }
}

impl From<dbcore::ConnectionConfig> for ConnectionConfig {
    fn from(c: dbcore::ConnectionConfig) -> Self {
        Self {
            id: c.id,
            name: c.name,
            group: c.group,
            kind: c.kind.into(),
            host: c.host,
            port: c.port,
            database: c.database,
            user: c.user,
            password: c.password,
            ssl_mode: c.ssl_mode.into(),
            show_all_databases: c.show_all_databases,
        }
    }
}

impl From<SslMode> for dbcore::SslMode {
    fn from(m: SslMode) -> Self {
        match m {
            SslMode::Disable => Self::Disable,
            SslMode::Prefer => Self::Prefer,
            SslMode::Require => Self::Require,
            SslMode::VerifyFull => Self::VerifyFull,
        }
    }
}

impl From<dbcore::SslMode> for SslMode {
    fn from(m: dbcore::SslMode) -> Self {
        match m {
            dbcore::SslMode::Disable => Self::Disable,
            dbcore::SslMode::Prefer => Self::Prefer,
            dbcore::SslMode::Require => Self::Require,
            dbcore::SslMode::VerifyFull => Self::VerifyFull,
        }
    }
}

impl From<TableKind> for dbcore::TableKind {
    fn from(k: TableKind) -> Self {
        match k {
            TableKind::Table => Self::Table,
            TableKind::View => Self::View,
        }
    }
}

impl From<dbcore::TableKind> for TableKind {
    fn from(k: dbcore::TableKind) -> Self {
        match k {
            dbcore::TableKind::Table => Self::Table,
            dbcore::TableKind::View => Self::View,
        }
    }
}

impl From<TableInfo> for dbcore::TableInfo {
    fn from(t: TableInfo) -> Self {
        Self { schema: t.schema, name: t.name, kind: t.kind.into(), estimated_row_count: t.estimated_row_count }
    }
}

impl From<dbcore::TableInfo> for TableInfo {
    fn from(t: dbcore::TableInfo) -> Self {
        Self { schema: t.schema, name: t.name, kind: t.kind.into(), estimated_row_count: t.estimated_row_count }
    }
}

impl From<dbcore::Schema> for Schema {
    fn from(s: dbcore::Schema) -> Self {
        Self { name: s.name, tables: s.tables.into_iter().map(Into::into).collect() }
    }
}

impl From<dbcore::ColumnInfo> for ColumnInfo {
    fn from(c: dbcore::ColumnInfo) -> Self {
        Self { name: c.name, type_name: c.type_name, is_primary_key: c.is_primary_key, is_nullable: c.is_nullable }
    }
}

impl From<ColumnInfo> for dbcore::ColumnInfo {
    fn from(c: ColumnInfo) -> Self {
        Self { name: c.name, type_name: c.type_name, is_primary_key: c.is_primary_key, is_nullable: c.is_nullable }
    }
}

impl From<dbcore::TableColumns> for TableColumns {
    fn from(t: dbcore::TableColumns) -> Self {
        Self { schema: t.schema, table: t.table, columns: t.columns.into_iter().map(Into::into).collect() }
    }
}

impl From<TableColumns> for dbcore::TableColumns {
    fn from(t: TableColumns) -> Self {
        Self { schema: t.schema, table: t.table, columns: t.columns.into_iter().map(Into::into).collect() }
    }
}

impl From<Schema> for dbcore::Schema {
    fn from(s: Schema) -> Self {
        Self { name: s.name, tables: s.tables.into_iter().map(Into::into).collect() }
    }
}

impl From<dbcore::Value> for Value {
    fn from(v: dbcore::Value) -> Self {
        match v {
            dbcore::Value::Null => Self::Null,
            dbcore::Value::Bool(b) => Self::Bool(b),
            dbcore::Value::Int(i) => Self::Int(i),
            dbcore::Value::Float(f) => Self::Float(f),
            dbcore::Value::Decimal(s) => Self::Decimal(s),
            dbcore::Value::Text(s) => Self::Text(s),
        }
    }
}

impl From<dbcore::QueryResult> for QueryResult {
    fn from(r: dbcore::QueryResult) -> Self {
        Self {
            columns: r.columns.into_iter().map(Into::into).collect(),
            rows: r.rows.into_iter().map(|row| row.into_iter().map(Into::into).collect()).collect(),
            total_count: r.total_count,
            rows_affected: r.rows_affected,
            truncated: r.truncated,
            origins: r
                .origins
                .into_iter()
                .map(|o| o.map(|o| results::ColumnOrigin { schema: o.schema, table: o.table, column: o.column }))
                .collect(),
        }
    }
}

impl From<RowQuery> for dbcore::RowQuery {
    fn from(q: RowQuery) -> Self {
        Self {
            sort: q.sort.into_iter().map(|k| dbcore::SortKey { column: k.column, descending: k.descending }).collect(),
            filter: q.filter,
        }
    }
}

impl From<dbcore::TableStructure> for TableStructure {
    fn from(s: dbcore::TableStructure) -> Self {
        Self {
            columns: s
                .columns
                .into_iter()
                .map(|c| ColumnDetail {
                    name: c.name,
                    type_name: c.type_name,
                    is_nullable: c.is_nullable,
                    default_value: c.default_value,
                    is_primary_key: c.is_primary_key,
                    comment: c.comment,
                })
                .collect(),
            primary_key: s.primary_key,
            indexes: s
                .indexes
                .into_iter()
                .map(|i| IndexInfo {
                    name: i.name,
                    columns: i.columns,
                    is_unique: i.is_unique,
                    is_primary: i.is_primary,
                    definition: i.definition,
                })
                .collect(),
            foreign_keys: s
                .foreign_keys
                .into_iter()
                .map(|f| ForeignKeyInfo {
                    name: f.name,
                    columns: f.columns,
                    referenced_schema: f.referenced_schema,
                    referenced_table: f.referenced_table,
                    referenced_columns: f.referenced_columns,
                    on_update: f.on_update,
                    on_delete: f.on_delete,
                })
                .collect(),
            referenced_by: s
                .referenced_by
                .into_iter()
                .map(|k| ReferencingKey {
                    schema: k.schema,
                    table: k.table,
                    name: k.name,
                    columns: k.columns,
                    referenced_columns: k.referenced_columns,
                })
                .collect(),
            ddl: s.ddl,
        }
    }
}

impl From<Value> for dbcore::Value {
    fn from(v: Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Int(i) => Self::Int(i),
            Value::Float(f) => Self::Float(f),
            Value::Decimal(s) => Self::Decimal(s),
            Value::Text(s) => Self::Text(s),
        }
    }
}

impl From<CellEdit> for dbcore::edit::CellEdit {
    fn from(e: CellEdit) -> Self {
        let value = match e.value {
            EditValue::Null => dbcore::edit::EditValue::Null,
            EditValue::Default => dbcore::edit::EditValue::Default,
            EditValue::Text { text } => dbcore::edit::EditValue::Text(text),
        };
        Self { column: e.column, value }
    }
}

impl From<KeyValue> for dbcore::edit::KeyValue {
    fn from(k: KeyValue) -> Self {
        Self { column: k.column, value: k.value.into() }
    }
}

impl From<RowChange> for dbcore::edit::RowChange {
    fn from(c: RowChange) -> Self {
        let keys = |key: Vec<KeyValue>| key.into_iter().map(Into::into).collect();
        let edits = |set: Vec<CellEdit>| set.into_iter().map(Into::into).collect();
        match c {
            RowChange::Update { key, set } => Self::Update { key: keys(key), set: edits(set) },
            RowChange::Insert { values } => Self::Insert { values: edits(values) },
            RowChange::Delete { key } => Self::Delete { key: keys(key) },
        }
    }
}

impl From<dbcore::edit::EditStatement> for EditStatement {
    fn from(s: dbcore::edit::EditStatement) -> Self {
        Self { sql: s.sql, expect_one_row: s.expect_one_row, target: s.target }
    }
}

impl From<dbcore::Error> for DbError {
    fn from(e: dbcore::Error) -> Self {
        use dbcore::Error as E;
        match e {
            E::ConnectionFailed(message) => Self::ConnectionFailed { message },
            E::TableNotFound(name) => Self::TableNotFound { name },
            E::Unsupported(message) => Self::Unsupported { message },
            E::Query(message) => Self::Query { message },
            E::Cancelled => Self::Cancelled,
            E::InvalidConfig(message) => Self::InvalidConfig { message },
            E::Storage(message) => Self::Storage { message },
            E::Internal(message) => Self::Internal { message },
        }
    }
}
