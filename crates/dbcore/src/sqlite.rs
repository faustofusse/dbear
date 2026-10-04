//! SQLite driver (rusqlite, bundled SQLite).
//!
//! rusqlite is synchronous, so every call runs on a blocking thread. Like the server drivers,
//! there are two connections to the file: one for browsing and one for scripts, so a long
//! script doesn't block browsing and `cancel` (`sqlite3_interrupt`) only hits scripts.
//!
//! "Schemas" are SQLite's attached databases: `main`, plus any the script `ATTACH`es.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, ErrorCode, InterruptHandle, OpenFlags, OptionalExtension};

use crate::dialect::{hex_preview, line_column, Dialect};
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{page_sql, CursorValue, Keyset, PageCursor, RowPage, SeekColumn, Start};
use crate::model::*;

const SQLITE: Dialect = Dialect(DatabaseKind::Sqlite);
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Files up to this size get exact row counts in the table list (counting is a full scan).
const COUNT_TABLES_UP_TO_BYTES: i64 = 64 * 1024 * 1024;

pub struct SqliteDriver {
    config: ConnectionConfig,
    browse: Arc<Session>,
    query: Arc<Session>,
}

#[derive(Default)]
struct Session {
    conn: Mutex<Option<rusqlite::Connection>>,
    /// Interrupts the running statement; usable while `conn` is locked by it.
    interrupt: Mutex<Option<InterruptHandle>>,
}

impl SqliteDriver {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { config, browse: Arc::default(), query: Arc::default() }
    }

    /// Runs `f` on a blocking thread with the session's connection, opening it first if needed.
    async fn run<T, F>(&self, session: &Arc<Session>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let session = session.clone();
        let path = database_path(&self.config.database);
        tokio::task::spawn_blocking(move || {
            let mut slot = lock(&session.conn);
            if slot.is_none() {
                let conn = open(&path)?;
                *lock(&session.interrupt) = Some(conn.get_interrupt_handle());
                *slot = Some(conn);
            }
            f(slot.as_ref().expect("opened above"))
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `~/x.db` → `/Users/me/x.db`.
pub(crate) fn database_path(database: &str) -> PathBuf {
    let database = database.trim();
    match (database.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(database),
    }
}

fn open(path: &std::path::Path) -> Result<rusqlite::Connection> {
    // Never create files: a typo in the path should be an error, not a new empty database.
    if !path.is_file() {
        return Err(Error::ConnectionFailed(format!("No database file at {}", path.display())));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, flags)
        .or_else(|_| {
            // Read-only files (or directories) still open for browsing.
            rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        })
        .map_err(|e| Error::ConnectionFailed(e.to_string()))?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| Error::ConnectionFailed(e.to_string()))?;
    // Fail now if the file isn't a database ("file is not a database"), not on first browse.
    conn.query_row("select count(*) from sqlite_master", [], |_| Ok(()))
        .map_err(|e| Error::ConnectionFailed(e.to_string()))?;
    Ok(conn)
}

#[async_trait]
impl Driver for SqliteDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.run(&self.browse, |_| Ok(())).await
    }

    async fn disconnect(&self) {
        self.cancel().await;
        for session in [&self.browse, &self.query] {
            let session = session.clone();
            // Waits for a running statement to stop (it was interrupted above).
            let _ = tokio::task::spawn_blocking(move || {
                lock(&session.interrupt).take();
                lock(&session.conn).take();
            })
            .await;
        }
    }

    async fn is_connected(&self) -> bool {
        // The interrupt handle exists exactly while a connection is open (and never blocks).
        lock(&self.browse.interrupt).is_some() || lock(&self.query.interrupt).is_some()
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        self.run(&self.browse, list_schemas).await
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        self.run(&self.browse, list_columns).await
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        let (table, query) = (table.clone(), query.clone());
        self.run(&self.browse, move |conn| fetch_rows(conn, &table, &query, limit, offset)).await
    }

    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let (table, query, after) = (table.clone(), query.clone(), after.cloned());
        self.run(&self.browse, move |conn| fetch_page(conn, &table, &query, limit, after.as_ref())).await
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        let table = table.clone();
        self.run(&self.browse, move |conn| describe(conn, &table)).await
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        let sql = sql.to_string();
        // A dropped future can't stop the blocking thread, but interrupting the statement does.
        let guard = InterruptOnDrop(Some(self.query.clone()));
        let result = self.run(&self.query, move |conn| run_script(conn, &sql, max_rows)).await;
        guard.disarm();
        result
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        // The browse connection is used exclusively while this runs, so nothing else joins the transaction.
        let statements = statements.to_vec();
        self.run(&self.browse, move |conn| apply(conn, &statements)).await
    }

    async fn cancel(&self) {
        if let Some(handle) = lock(&self.query.interrupt).as_ref() {
            handle.interrupt();
        }
    }
}

struct InterruptOnDrop(Option<Arc<Session>>);

impl InterruptOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        if let Some(session) = self.0.take() {
            if let Some(handle) = lock(&session.interrupt).as_ref() {
                handle.interrupt();
            }
        }
    }
}

// MARK: Catalog

fn list_schemas(conn: &rusqlite::Connection) -> Result<Vec<Schema>> {
    let databases: Vec<String> = conn
        .prepare("select name from pragma_database_list order by seq")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| query_error(e, None))?;

    let small = file_size(conn).is_some_and(|size| size <= COUNT_TABLES_UP_TO_BYTES);
    let mut schemas = Vec::new();
    for database in databases {
        let sql = format!(
            "select name, type from {}.sqlite_master where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' order by name",
            SQLITE.quote_ident(&database)
        );
        let rows: Vec<(String, String)> = conn
            .prepare(&sql)
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
            .map_err(|e| query_error(e, None))?;
        // `temp` only matters once something was created in it.
        if database == "temp" && rows.is_empty() {
            continue;
        }
        let tables = rows
            .into_iter()
            .map(|(name, kind)| {
                let kind = if kind == "view" { TableKind::View } else { TableKind::Table };
                let estimated_row_count = (small && kind == TableKind::Table)
                    .then(|| count(conn, &SQLITE.count_query(&SQLITE.quote_relation(&database, &name), None)).ok())
                    .flatten();
                TableInfo { schema: database.clone(), name, kind, estimated_row_count }
            })
            .collect();
        schemas.push(Schema { name: database, tables });
    }
    Ok(schemas)
}

fn list_columns(conn: &rusqlite::Connection) -> Result<Vec<TableColumns>> {
    let databases: Vec<String> = conn
        .prepare("select name from pragma_database_list order by seq")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| query_error(e, None))?;

    let mut tables: Vec<TableColumns> = Vec::new();
    for database in databases {
        let schema_ident = SQLITE.quote_ident(&database);
        let sql = format!(
            "select m.name, ti.name, ti.type, ti.\"notnull\", ti.pk
             from {schema_ident}.sqlite_master m, pragma_table_info(m.name, '{database}') ti
             where m.type in ('table', 'view') and m.name not like 'sqlite\\_%' escape '\\'
             order by m.name, ti.cid"
        );
        let rows: Vec<(String, String, String, bool, i64)> = match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .and_then(|rows| rows.collect())
                .map_err(|e| query_error(e, None))?,
            // `temp` may have nothing attached yet; a missing schema also lands here.
            Err(_) => continue,
        };
        for (table, column, type_name, not_null, pk) in rows {
            if tables.last().is_none_or(|t| t.schema != database || t.table != table) {
                tables.push(TableColumns { schema: database.clone(), table, columns: Vec::new() });
            }
            tables.last_mut().unwrap().columns.push(ColumnInfo {
                name: column,
                type_name: type_name.to_lowercase(),
                is_primary_key: pk > 0,
                is_nullable: !not_null && pk == 0,
            });
        }
    }
    Ok(tables)
}

fn file_size(conn: &rusqlite::Connection) -> Option<i64> {
    conn.query_row("select page_count * page_size from pragma_page_count, pragma_page_size", [], |r| r.get(0)).ok()
}

/// Runs a `select count(*) …` query.
fn count(conn: &rusqlite::Connection, sql: &str) -> Result<u64> {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
        .map(|n| n as u64)
        .map_err(|e| query_error(e, None))
}

struct TableMeta {
    kind: TableKind,
    columns: Vec<ColumnInfo>,
    /// Primary key columns in key order.
    primary_key: Vec<String>,
    without_rowid: bool,
    /// Columns declared NOT NULL (primary key columns are reported non-nullable regardless).
    not_null: Vec<String>,
}

fn table_meta(conn: &rusqlite::Connection, table: &TableInfo) -> Result<TableMeta> {
    let schema = SQLITE.quote_ident(&table.schema);
    let master = format!("select type, sql from {schema}.sqlite_master where name = ?1 and type in ('table', 'view')");
    let (kind, ddl): (String, Option<String>) = conn
        .query_row(&master, [&table.name], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::TableNotFound(table.qualified_name()),
            e => query_error(e, None),
        })?;

    let sql = "select name, type, \"notnull\", pk from pragma_table_info(?1, ?2) order by cid";
    let mut stmt = conn.prepare(sql).map_err(|e| query_error(e, None))?;
    let rows: Vec<(String, String, bool, i64)> = stmt
        .query_map([&table.name, &table.schema], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .and_then(|rows| rows.collect())
        .map_err(|e| query_error(e, None))?;

    let mut keyed: Vec<(i64, String)> = rows.iter().filter(|r| r.3 > 0).map(|r| (r.3, r.0.clone())).collect();
    keyed.sort();
    let not_null = rows.iter().filter(|r| r.2).map(|r| r.0.clone()).collect();
    Ok(TableMeta {
        not_null,
        kind: if kind == "view" { TableKind::View } else { TableKind::Table },
        columns: rows
            .into_iter()
            .map(|(name, type_name, not_null, pk)| ColumnInfo {
                name,
                type_name: type_name.to_lowercase(),
                is_primary_key: pk > 0,
                is_nullable: !not_null && pk == 0,
            })
            .collect(),
        primary_key: keyed.into_iter().map(|(_, name)| name).collect(),
        without_rowid: ddl.is_some_and(|d| d.to_ascii_lowercase().contains("without rowid")),
    })
}

fn fetch_rows(conn: &rusqlite::Connection, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
    let meta = table_meta(conn, table)?;
    let relation = SQLITE.quote_relation(&table.schema, &table.name);
    let keyset = keyset_for(&meta, table, query)?;
    let filter = query.filter.as_deref();
    let sql = page_sql(&relation, &[], filter, None, &keyset.order_by(), u64::from(limit), offset);
    // A filter is one expression, but `run_script` would happily run a second statement: prepare just one.
    let (mut result, _) = run_statement(conn, &sql, &[], &[])?;
    result.columns = meta.columns;
    if offset == 0 && meta.kind == TableKind::Table {
        result.total_count = Some(count(conn, &SQLITE.count_query(&relation, filter))?);
    }
    Ok(result)
}

/// Like `fetch_rows`, but seeks past the cursor's last row (bound as parameters) when the table
/// has a usable row id or key.
fn fetch_page(conn: &rusqlite::Connection, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
    let meta = table_meta(conn, table)?;
    let relation = SQLITE.quote_relation(&table.schema, &table.name);
    let keyset = keyset_for(&meta, table, query)?;
    let filter = query.filter.as_deref();
    let width = meta.columns.len();
    let extra: Vec<String> = keyset.columns.iter().filter(|c| c.index >= width).map(|c| c.expr.clone()).collect();

    // `?N` is the N-th sort term, however often the predicate repeats it.
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    let start = keyset.start(after, |i, v| {
        if params.len() <= i {
            params.resize(i + 1, rusqlite::types::Value::Null);
        }
        params[i] = to_sql(v);
        format!("?{}", i + 1)
    });
    let (segments, offset) = match start {
        Start::Offset(n) => (vec![None], n),
        Start::Seek(segments) => (segments.into_iter().map(Some).collect(), 0),
        Start::Empty => {
            let result = QueryResult { columns: meta.columns, ..Default::default() };
            return Ok(RowPage { result, next: None });
        }
    };
    let (mut result, mut keys) = (QueryResult::default(), Vec::new());
    let want = limit as usize + 1;
    for seek in segments {
        let need = want.saturating_sub(result.rows.len()) as u64;
        if need == 0 {
            break;
        }
        let sql = page_sql(&relation, &extra, filter, seek.as_deref(), &keyset.order_by(), need, offset);
        let (part, part_keys) = run_statement(conn, &sql, &params, &keyset.key_indexes())?;
        result.rows.extend(part.rows);
        keys.extend(part_keys);
    }
    result.columns = meta.columns;
    if after.is_none() && meta.kind == TableKind::Table {
        result.total_count = Some(count(conn, &SQLITE.count_query(&relation, filter))?);
    }
    Ok(keyset.finish(result, keys, width, limit, after))
}

/// The order of a table's pages: the user's sort, then `rowid` (insertion order for most tables
/// and always indexed) or the primary key. Seeking needs that tiebreak to be unique and not
/// shadowed by a column of the same name; views always page with OFFSET.
fn keyset_for(meta: &TableMeta, table: &TableInfo, query: &RowQuery) -> Result<Keyset> {
    let width = meta.columns.len();
    let shadowed = meta.columns.iter().any(|c| ["rowid", "_rowid_", "oid"].iter().any(|r| c.name.eq_ignore_ascii_case(r)));
    let key_columns = || meta.primary_key.iter().filter_map(|c| SeekColumn::column(SQLITE, &meta.columns, c));
    let (tiebreak, enabled): (Vec<SeekColumn>, bool) = match meta.kind {
        TableKind::Table if !meta.without_rowid && meta.primary_key.len() != 1 => (vec![SeekColumn::row_id("rowid", width)], !shadowed),
        // A lone INTEGER PRIMARY KEY is the rowid itself. Other single keys can hold NULLs (an old
        // SQLite quirk), so `rowid` goes after them to keep the order unique.
        TableKind::Table if !meta.without_rowid && !meta.columns.iter().any(|c| c.is_primary_key && c.type_name == "integer") => {
            let mut keys: Vec<SeekColumn> = key_columns().map(|c| SeekColumn { nullable: true, ..c }).collect();
            keys.push(SeekColumn::row_id("rowid", width));
            (keys, !shadowed)
        }
        TableKind::Table => (key_columns().collect(), true),
        TableKind::View => (Vec::new(), false),
    };
    // Keys declared without NOT NULL may hold NULLs in SQLite: say so, so seeks handle them.
    let mut columns = meta.columns.clone();
    for c in &mut columns {
        if c.is_primary_key && meta.kind == TableKind::Table && !meta.without_rowid && c.type_name != "integer" {
            c.is_nullable = !meta.not_null.contains(&c.name);
        }
    }
    Keyset::new(SQLITE, table, query, &columns, tiebreak, enabled)
}

fn to_sql(v: &CursorValue) -> rusqlite::types::Value {
    use rusqlite::types::Value as V;
    match v {
        CursorValue::Null => V::Null,
        CursorValue::Int(i) => V::Integer(*i),
        CursorValue::Float(bits) => V::Real(f64::from_bits(*bits)),
        CursorValue::Text(t) => V::Text(t.clone()),
        CursorValue::Bytes(b) => V::Blob(b.clone()),
    }
}

/// The exact value (storage class included) of a sort key, for the next page's seek.
fn key_value(v: ValueRef<'_>) -> CursorValue {
    match v {
        ValueRef::Null => CursorValue::Null,
        ValueRef::Integer(i) => CursorValue::Int(i),
        ValueRef::Real(f) => CursorValue::float(f),
        ValueRef::Text(t) => CursorValue::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => CursorValue::Bytes(b.to_vec()),
    }
}

// MARK: Editing

fn apply(conn: &rusqlite::Connection, statements: &[EditStatement]) -> Result<u64> {
    // `immediate` takes the write lock now, so a busy database fails before anything runs.
    conn.execute_batch("begin immediate").map_err(|e| query_error(e, None))?;
    let mut total = 0;
    for statement in statements {
        let result = match conn.execute(&statement.sql, []) {
            Ok(affected) => edit::check_affected(statement, affected as u64).map(|()| affected as u64),
            Err(e) => Err(edit::failed(statement, query_error(e, None))),
        };
        match result {
            Ok(affected) => total += affected,
            Err(e) => {
                let _ = conn.execute_batch("rollback");
                return Err(e);
            }
        }
    }
    conn.execute_batch("commit").map_err(|e| {
        let _ = conn.execute_batch("rollback");
        query_error(e, None)
    })?;
    Ok(total)
}

// MARK: Structure

fn describe(conn: &rusqlite::Connection, table: &TableInfo) -> Result<TableStructure> {
    let meta = table_meta(conn, table)?;
    let schema = SQLITE.quote_ident(&table.schema);
    let err = |e| query_error(e, None);

    let defaults: Vec<Option<String>> = conn
        .prepare("select dflt_value from pragma_table_info(?1, ?2) order by cid")
        .and_then(|mut s| s.query_map([&table.name, &table.schema], |r| r.get(0))?.collect())
        .map_err(err)?;
    let columns = meta
        .columns
        .iter()
        .zip(defaults)
        .map(|(c, default_value)| ColumnDetail {
            name: c.name.clone(),
            type_name: c.type_name.clone(),
            is_nullable: c.is_nullable,
            default_value,
            is_primary_key: c.is_primary_key,
            comment: None,
        })
        .collect();

    let index_rows: Vec<(String, bool, String)> = conn
        .prepare("select name, \"unique\", origin from pragma_index_list(?1, ?2) order by origin = 'pk' desc, name")
        .and_then(|mut s| s.query_map([&table.name, &table.schema], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect())
        .map_err(err)?;
    let mut indexes = Vec::new();
    for (name, is_unique, origin) in index_rows {
        let columns: Vec<String> = conn
            .prepare("select coalesce(name, '<expression>') from pragma_index_info(?1, ?2) order by seqno")
            .and_then(|mut s| s.query_map([&name, &table.schema], |r| r.get(0))?.collect())
            .map_err(err)?;
        // Automatic indexes (primary key / unique constraints) have no SQL of their own, and a
        // WITHOUT ROWID table's primary key isn't in `sqlite_master` at all.
        let definition: Option<String> = conn
            .query_row(&format!("select sql from {schema}.sqlite_master where type = 'index' and name = ?1"), [&name], |r| r.get(0))
            .optional()
            .map_err(err)?
            .flatten();
        indexes.push(IndexInfo { name, columns, is_unique, is_primary: origin == "pk", definition });
    }

    let fk_rows: Vec<(i64, String, String, Option<String>, String, String)> = conn
        .prepare("select id, \"table\", \"from\", \"to\", on_update, on_delete from pragma_foreign_key_list(?1, ?2) order by id, seq")
        .and_then(|mut s| {
            s.query_map([&table.name, &table.schema], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect()
        })
        .map_err(err)?;
    let mut foreign_keys: Vec<(i64, ForeignKeyInfo)> = Vec::new();
    for (id, parent, from, to, on_update, on_delete) in fk_rows {
        if foreign_keys.last().is_none_or(|(last, _)| *last != id) {
            let fk = ForeignKeyInfo {
                name: String::new(),
                columns: Vec::new(),
                referenced_schema: table.schema.clone(),
                referenced_table: parent,
                referenced_columns: Vec::new(),
                on_update,
                on_delete,
            };
            foreign_keys.push((id, fk));
        }
        let fk = &mut foreign_keys.last_mut().expect("pushed above").1;
        fk.columns.push(from);
        if let Some(to) = to {
            fk.referenced_columns.push(to);
        }
    }

    let incoming_rows: Vec<(String, i64, String, Option<String>)> = conn
        .prepare(&referenced_by_query(&table.schema))
        .and_then(|mut s| s.query_map([&table.name, &table.schema], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
        .map_err(err)?;

    // The table's own SQL, then its indexes and triggers, as SQLite stored them.
    let statements: Vec<String> = conn
        .prepare(&format!(
            "select sql from {schema}.sqlite_master where tbl_name = ?1 and sql is not null
             order by case type when 'table' then 0 when 'view' then 0 when 'index' then 1 else 2 end, name"
        ))
        .and_then(|mut s| s.query_map([&table.name], |r| r.get(0))?.collect())
        .map_err(err)?;
    let ddl = (!statements.is_empty()).then(|| statements.iter().map(|s| format!("{};", s.trim_end_matches(';'))).collect::<Vec<_>>().join("\n\n"));

    Ok(TableStructure {
        columns,
        primary_key: meta.primary_key,
        indexes,
        foreign_keys: foreign_keys.into_iter().map(|(_, fk)| fk).collect(),
        referenced_by: group_referenced_by(&table.schema, incoming_rows),
        ddl,
    })
}

/// Foreign keys of the other tables in `schema` that point at table `?1` (`?2` = `schema`):
/// rows of `(table, key id, column, referenced column)`. Also used by the libSQL driver.
pub(crate) fn referenced_by_query(schema: &str) -> String {
    format!(
        "select m.name, f.id, f.\"from\", f.\"to\"
         from {}.sqlite_master m join pragma_foreign_key_list(m.name, ?2) f
         where m.type = 'table' and f.\"table\" = ?1 collate nocase
         order by m.name, f.id, f.seq",
        SQLITE.quote_ident(schema)
    )
}

/// Groups [`referenced_by_query`] rows into keys. A NULL referenced column means the key points at
/// the primary key implicitly (`referenced_columns` stays empty).
pub(crate) fn group_referenced_by(schema: &str, rows: impl IntoIterator<Item = (String, i64, String, Option<String>)>) -> Vec<ReferencingKey> {
    let mut keys: Vec<(i64, ReferencingKey)> = Vec::new();
    for (table, id, from, to) in rows {
        if keys.last().is_none_or(|(last, k)| *last != id || k.table != table) {
            let key = ReferencingKey { schema: schema.into(), table, name: String::new(), columns: Vec::new(), referenced_columns: Vec::new() };
            keys.push((id, key));
        }
        let key = &mut keys.last_mut().expect("pushed above").1;
        key.columns.push(from);
        if let Some(to) = to {
            key.referenced_columns.push(to);
        }
    }
    keys.into_iter().map(|(_, k)| k).collect()
}

// MARK: Running SQL

/// Runs exactly one statement (rusqlite rejects anything after it) and returns its rows, plus the
/// raw values at `keys` of every row (for keyset cursors). Errors leave out positions: they'd
/// point into the generated query, not at the user's filter.
fn run_statement(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[rusqlite::types::Value],
    keys: &[usize],
) -> Result<(QueryResult, Vec<Vec<CursorValue>>)> {
    let unlocated = |e| match e {
        rusqlite::Error::SqlInputError { error, msg, .. } if error.code != ErrorCode::OperationInterrupted => {
            Error::Query(format!("ERROR: {msg}"))
        }
        rusqlite::Error::MultipleStatement => Error::Query("A filter is a single condition: remove the “;”.".into()),
        e => query_error(e, None),
    };
    let mut stmt = conn.prepare(sql).map_err(unlocated)?;
    let types: Vec<String> =
        stmt.columns().iter().map(|c| c.decl_type().unwrap_or_default().to_lowercase()).collect();
    // A segment may not use every key (`?N` is the N-th term): bind as many as it has.
    let params = &params[..params.len().min(stmt.parameter_count())];
    let mut rows = stmt.query(rusqlite::params_from_iter(params)).map_err(unlocated)?;
    let (mut result, mut key_rows) = (QueryResult::default(), Vec::new());
    while let Some(row) = rows.next().map_err(unlocated)? {
        let values = (0..types.len()).map(|i| row.get_ref(i).map(|v| decode(v, &types[i]))).collect::<rusqlite::Result<_>>();
        result.rows.push(values.map_err(unlocated)?);
        if !keys.is_empty() {
            let values = keys.iter().map(|&i| row.get_ref(i).map(key_value)).collect::<rusqlite::Result<_>>();
            key_rows.push(values.map_err(unlocated)?);
        }
    }
    Ok((result, key_rows))
}

/// Runs every statement in order; returns the last result set, or the changes of the last statement.
fn run_script(conn: &rusqlite::Connection, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let mut batch = Batch::new(conn, sql);
    let mut last_rows: Option<QueryResult> = None;
    let mut last_affected: Option<u64> = None;

    while let Some(mut stmt) = batch.next().map_err(|e| query_error(e, Some(sql)))? {
        if stmt.column_count() == 0 {
            let changes = stmt.raw_execute().map_err(|e| query_error(e, Some(sql)))?;
            last_affected = Some(changes as u64);
            continue;
        }
        let columns: Vec<ColumnInfo> = stmt
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_name: c.decl_type().unwrap_or_default().to_lowercase(),
                is_primary_key: false,
                is_nullable: true,
            })
            .collect();
        let mut result = QueryResult { columns, ..Default::default() };
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(|e| query_error(e, Some(sql)))? {
            if result.rows.len() < max_rows {
                let values = result
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| row.get_ref(i).map(|v| decode(v, &c.type_name)))
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| query_error(e, Some(sql)))?;
                result.rows.push(values);
            } else {
                result.truncated = true;
                *result.total_count.get_or_insert(max_rows as u64) += 1;
            }
        }
        last_rows = Some(result);
    }
    Ok(last_rows.unwrap_or(QueryResult { rows_affected: last_affected, ..Default::default() }))
}

/// SQLite values are dynamically typed; the declared column type refines booleans and decimals.
pub(crate) fn decode(value: ValueRef<'_>, declared: &str) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) if declared.contains("bool") => Value::Bool(i != 0),
        ValueRef::Integer(i) => Value::Int(i),
        // NUMERIC affinity stores '19.90' as the REAL 19.9; still right-aligned and exact-looking.
        ValueRef::Real(f) if is_decimal(declared) => Value::Decimal(f.to_string()),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) if is_decimal(declared) => Value::Decimal(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Text(hex_preview(b)),
    }
}

fn is_decimal(declared: &str) -> bool {
    declared.starts_with("decimal") || declared.starts_with("numeric")
}

// MARK: Errors

fn query_error(e: rusqlite::Error, script: Option<&str>) -> Error {
    match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted => Error::Cancelled,
        rusqlite::Error::SqlInputError { error, msg, sql, offset } => {
            if error.code == ErrorCode::OperationInterrupted {
                return Error::Cancelled;
            }
            // `sql` is the rest of the script from the failing statement on; `offset` is in bytes.
            let full = script.unwrap_or(&sql);
            let start = full.len().saturating_sub(sql.len());
            let byte = (start + offset.max(0) as usize).min(full.len());
            let chars = full.get(..byte).map_or(0, |s| s.chars().count());
            let (line, column) = line_column(full, chars);
            Error::Query(format!("ERROR: {msg} (line {line}, column {column})"))
        }
        rusqlite::Error::SqliteFailure(_, Some(msg)) => Error::Query(format!("ERROR: {msg}")),
        e => Error::Query(format!("ERROR: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_by_declared_type() {
        assert_eq!(decode(ValueRef::Integer(1), "boolean"), Value::Bool(true));
        assert_eq!(decode(ValueRef::Integer(7), "integer"), Value::Int(7));
        assert_eq!(decode(ValueRef::Text(b"1.10"), "decimal(10,2)"), Value::Decimal("1.10".into()));
        assert_eq!(decode(ValueRef::Real(19.9), "numeric"), Value::Decimal("19.9".into()));
        assert_eq!(decode(ValueRef::Blob(&[1, 2]), "blob"), Value::Text("0x0102".into()));
        assert_eq!(decode(ValueRef::Null, "text"), Value::Null);
    }

    #[test]
    fn expands_home() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(database_path("~/a.db"), PathBuf::from(home).join("a.db"));
        }
        assert_eq!(database_path(" /tmp/a.db "), PathBuf::from("/tmp/a.db"));
    }
}
