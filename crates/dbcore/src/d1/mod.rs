//! Cloudflare D1 driver, over Cloudflare's REST API ([`client`]). No Node, wrangler or local
//! files: an account ID, a database and an API token are enough, from any platform.
//!
//! The SQL is SQLite's, so catalog queries, decoding, keyset paging and edits follow the libSQL
//! driver (whose catalog this reuses). What differs:
//! - Every request is its own transaction, and D1 refuses `BEGIN`/`COMMIT`. A script runs as one
//!   batch: all or nothing. Edits run as one batch too, each followed by a check that it touched
//!   exactly one row, which fails the batch (and so rolls it back) when it didn't.
//! - Table pages select `quote(column)` and parse the literals back, so values are exact (the
//!   API's JSON would round big integers and turn blobs into arrays). Script results can't be
//!   rewritten that way and come back as JSON (see [`client`]).
//! - Results arrive whole: `max_rows` only trims what's kept. No row counts (D1 bills rows read).
//! - `cancel` drops the request; a statement already running on D1 still finishes.
//! - D1's internal tables (`_cf_*`) can't be read and are left out.
//! - The account's other databases are its "databases" (`list_databases`).
//!
//! Local D1 databases (`wrangler dev`) are SQLite files: see [`discover`].

pub(crate) mod client;
pub mod discover;
pub(crate) mod url;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::dialect::line_column;
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{page_sql_with, CursorValue, PageCursor, RowPage, Start};
use crate::libsql::catalog::{self, SQLITE};
use crate::libsql::hrana::{Arg, HValue, Stmt, StmtResult};
use crate::libsql::script;
use crate::model::*;
use client::{Client, D1Stmt, SqlFailure};

pub struct D1Driver {
    config: ConnectionConfig,
    /// Created on first use; dropped by `disconnect`.
    client: Mutex<Option<Arc<Client>>>,
    connected: AtomicBool,
    /// Fired by `cancel`; the running `execute` stops waiting and returns `Cancelled`.
    cancel: Notify,
}

/// Leaves out SQLite's tables and the ones D1 keeps for itself (`_cf_KV`, `_cf_METADATA`), which
/// can't be read. `name` is the column holding the table's name.
fn visible(name: &str) -> String {
    format!("{name} not like 'sqlite\\_%' escape '\\' and {name} not like '\\_cf\\_%' escape '\\'")
}

impl D1Driver {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { config, client: Mutex::new(None), connected: AtomicBool::new(false), cancel: Notify::new() }
    }

    fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = Arc::new(Client::new(&self.config)?);
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Notes whether D1 answered, for `is_connected`.
    fn track<T>(&self, result: Result<T>) -> Result<T> {
        match &result {
            Ok(_) => self.connected.store(true, Ordering::Relaxed),
            Err(Error::ConnectionFailed(_)) => self.connected.store(false, Ordering::Relaxed),
            Err(_) => {}
        }
        result
    }

    /// Runs statements in one request (one transaction): a result each.
    async fn run(&self, statements: &[D1Stmt]) -> Result<Vec<StmtResult>> {
        let result = async { Ok(self.client()?.run(statements).await?) }.await;
        self.track(result)
    }

    async fn run_one(&self, statement: D1Stmt) -> Result<StmtResult> {
        self.run(std::slice::from_ref(&statement)).await?.pop().ok_or_else(|| Error::Internal("D1 returned no result".into()))
    }

    async fn meta(&self, table: &TableInfo) -> Result<catalog::TableMeta> {
        let [master, info] = catalog::table_meta_statements(table).map(d1_stmt);
        let mut results = self.run(&[master, info]).await?.into_iter();
        let (Some(master), Some(info)) = (results.next(), results.next()) else {
            return Err(Error::Internal("D1 returned too few results".into()));
        };
        catalog::table_meta(table, master, info)
    }

    /// One page of `table`'s rows, every column (and `extra` key) selected through `quote()`.
    async fn page(&self, meta: &catalog::TableMeta, relation: &str, extra: &[String], query: &RowQuery, seek: Option<&str>, order_by: &[String], limit: u64, offset: u64) -> Result<StmtResult> {
        let select = quoted_columns(&meta.columns, extra);
        let sql = page_sql_with(&select, relation, query.filter.as_deref(), seek, order_by, limit, offset);
        let mut result = self.run_one(D1Stmt::new(sql)).await?;
        client::unquote_rows(&mut result);
        Ok(result)
    }
}

/// A catalog statement (text arguments only) for D1.
fn d1_stmt(stmt: Stmt) -> D1Stmt {
    let params = stmt
        .args
        .into_iter()
        .map(|arg| match arg {
            Arg::Text { value } | Arg::Integer { value } => value,
            Arg::Float { value } => value.to_string(),
            Arg::Null | Arg::Blob { .. } => String::new(),
        })
        .collect();
    D1Stmt { sql: stmt.sql, params }
}

/// `quote("a"), quote("b"), quote(rowid)`: exact values, as SQL literals.
fn quoted_columns(columns: &[ColumnInfo], extra: &[String]) -> String {
    let names = columns.iter().map(|c| SQLITE.quote_ident(&c.name)).chain(extra.iter().cloned());
    names.map(|expr| format!("quote({expr})")).collect::<Vec<_>>().join(", ")
}

/// A keyset cursor value as a SQLite literal (D1 binds numeric parameters as REAL, so none are bound).
fn literal(value: &CursorValue) -> String {
    let mut out = String::new();
    match value {
        CursorValue::Null => out.push_str("NULL"),
        CursorValue::Int(i) => out.push_str(&i.to_string()),
        CursorValue::Float(bits) => {
            let f = f64::from_bits(*bits);
            // SQLite reads these as ±Inf.
            out.push_str(&if f.is_nan() { "NULL".into() } else if f.is_infinite() { if f > 0.0 { "9e999".into() } else { "-9e999".into() } } else { crate::dump::literal::sqlite_float(f) });
        }
        CursorValue::Text(t) => crate::dump::literal::sqlite_text(&mut out, t.as_bytes()),
        CursorValue::Bytes(b) => crate::dump::literal::sqlite_blob(&mut out, b),
    }
    out
}

fn list_tables() -> D1Stmt {
    D1Stmt::new(format!("select name, type from sqlite_master where type in ('table', 'view') and {} order by name", visible("name")))
}

fn list_columns() -> D1Stmt {
    D1Stmt::new(format!(
        "select m.name, ti.name, ti.type, ti.\"notnull\", ti.pk
         from sqlite_master m, pragma_table_info(m.name) ti
         where m.type in ('table', 'view') and {}
         order by m.name, ti.cid",
        visible("m.name")
    ))
}

/// Foreign keys of other tables pointing at `?1` (like [`crate::sqlite::referenced_by_query`],
/// without D1's internal tables).
fn referenced_by(table: &TableInfo) -> D1Stmt {
    D1Stmt::with_args(
        format!(
            "select m.name, f.id, f.\"from\", f.\"to\"
             from {}.sqlite_master m join pragma_foreign_key_list(m.name, ?2) f
             where m.type = 'table' and {} and f.\"table\" = ?1 collate nocase
             order by m.name, f.id, f.seq",
            SQLITE.quote_ident(&table.schema),
            visible("m.name")
        ),
        &[&table.name, &table.schema],
    )
}

/// After each edit that must touch exactly one row: fails the batch otherwise, with an error
/// naming the edit and the rows it touched (`bad JSON path: 'dbear-edit:3:0'`).
fn edit_check(index: usize) -> D1Stmt {
    D1Stmt::new(format!("select case when changes() = 1 then 1 else json_extract('{{}}', 'dbear-edit:{index}:' || changes()) end"))
}

/// The edit and row count in an [`edit_check`] failure.
fn failed_check(message: &str) -> Option<(usize, u64)> {
    let rest = &message[message.find("dbear-edit:")? + 11..];
    let mut parts = rest.split(':');
    let index = parts.next()?.parse().ok()?;
    let count = parts.next()?.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()?;
    Some((index, count))
}

#[async_trait]
impl Driver for D1Driver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.run_one(D1Stmt::new("select 1")).await.map(|_| ())
    }

    async fn disconnect(&self) {
        self.cancel().await;
        self.client.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The account's databases, by name.
    async fn list_databases(&self) -> Result<Vec<String>> {
        let result = async { Ok(self.client()?.list_databases().await?) }.await;
        let mut names: Vec<String> = self.track(result)?.into_iter().map(|d| d.name).collect();
        let current = self.config.database.trim();
        if !current.is_empty() && !url::is_uuid(current) && !names.iter().any(|n| n == current) {
            names.push(current.to_string());
            names.sort();
        }
        Ok(names)
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        Ok(catalog::schemas(self.run_one(list_tables()).await?))
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        Ok(catalog::columns(self.run_one(list_columns()).await?))
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        let meta = self.meta(table).await?;
        let relation = SQLITE.quote_relation(&table.schema, &table.name);
        // Same order as `fetch_page`, so OFFSET and keyset pages agree.
        let keyset = catalog::keyset_for(&meta, table, query)?;
        let page = self.page(&meta, &relation, &[], query, None, &keyset.order_by(), u64::from(limit), offset).await?;
        // No `total_count`: D1 bills every row read, and `count(*)` reads the whole table.
        Ok(QueryResult { rows: catalog::decode_rows(&page, &meta.columns), columns: meta.columns, ..Default::default() })
    }

    /// Like `fetch_rows`, but seeks past the cursor's last row (written into the SQL as literals)
    /// when the table has a usable row id or key, as the SQLite driver does.
    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let meta = self.meta(table).await?;
        let relation = SQLITE.quote_relation(&table.schema, &table.name);
        let keyset = catalog::keyset_for(&meta, table, query)?;
        let width = meta.columns.len();
        let extra: Vec<String> = keyset.columns.iter().filter(|c| c.index >= width).map(|c| c.expr.clone()).collect();
        let (segments, offset) = match keyset.start(after, |_, v| literal(v)) {
            Start::Offset(n) => (vec![None], n),
            Start::Seek(segments) => (segments.into_iter().map(Some).collect(), 0),
            Start::Empty => {
                let result = QueryResult { columns: meta.columns, ..Default::default() };
                return Ok(RowPage { result, next: None });
            }
        };
        let (mut rows, mut keys) = (Vec::new(), Vec::new());
        let want = limit as usize + 1;
        let key_indexes = keyset.key_indexes();
        let order_by = keyset.order_by();
        for seek in segments {
            let need = want.saturating_sub(rows.len()) as u64;
            if need == 0 {
                break;
            }
            let part = self.page(&meta, &relation, &extra, query, seek.as_deref(), &order_by, need, offset).await?;
            keys.extend(part.rows.iter().map(|row| key_indexes.iter().map(|&i| row.get(i).map_or(CursorValue::Null, HValue::to_cursor)).collect()));
            rows.extend(catalog::decode_rows(&part, &meta.columns));
        }
        let result = QueryResult { rows, columns: meta.columns, ..Default::default() };
        Ok(keyset.finish(result, keys, width, limit, after))
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        let [master, info] = catalog::table_meta_statements(table).map(d1_stmt);
        let [indexes, foreign_keys, ddl, _] = catalog::structure_statements(table).map(d1_stmt);
        let all = [master.clone(), info.clone(), indexes, foreign_keys, ddl, referenced_by(table)];
        // A request fails as a whole: if an optional part fails, show at least the columns.
        let results = match self.run(&all).await {
            Ok(results) => results,
            Err(Error::Query(_)) => self.run(&[master, info]).await?,
            Err(e) => return Err(e),
        };
        let mut results = results.into_iter();
        let (Some(master), Some(info)) = (results.next(), results.next()) else {
            return Err(Error::Internal("D1 returned too few results".into()));
        };
        let meta = catalog::table_meta(table, master, info)?;
        let (indexes, foreign_keys, ddl, referenced_by) = (results.next(), results.next(), results.next(), results.next());
        Ok(catalog::structure(table, meta, indexes, foreign_keys, ddl, referenced_by))
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        let statements = script::split(sql);
        if statements.is_empty() {
            return Ok(QueryResult::default());
        }
        let client = self.client()?;
        let cancelled = self.cancel.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();

        // `changes()` at the end: D1's own count also includes rows changed by triggers.
        let batch: Vec<D1Stmt> = statements.iter().map(|s| D1Stmt::new(s.sql)).chain([D1Stmt::new("select changes()")]).collect();
        let result = tokio::select! {
            result = client.run(&batch) => result,
            _ = &mut cancelled => return Err(Error::Cancelled),
        };
        let result = result.map(|results| script_result(results, max_rows)).map_err(|failure| {
            // D1 only says where in its statement an error is, not which statement: locate it when there's one.
            match (&statements[..], failure.offset) {
                ([only], Some(offset)) => located(failure.error, sql, only.start + offset),
                _ => failure.error,
            }
        });
        self.track(result)
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let mut batch = Vec::new();
        let mut positions = Vec::new();
        for (i, statement) in statements.iter().enumerate() {
            positions.push(batch.len());
            batch.push(D1Stmt::new(statement.sql.trim().trim_end_matches(';')));
            if statement.expect_one_row {
                batch.push(edit_check(i));
            }
        }
        match self.run(&batch).await {
            Ok(results) => Ok(positions.iter().filter_map(|&p| results.get(p)).map(|r| r.affected_row_count).sum()),
            Err(Error::Query(message)) => Err(match failed_check(&message) {
                Some((i, count)) if i < statements.len() => match edit::check_affected(&statements[i], count) {
                    Err(e) => e,
                    Ok(()) => Error::Query(format!("Couldn’t save the changes:\n{message}\nNothing was saved.")),
                },
                _ if statements.len() == 1 => edit::failed(&statements[0], Error::Query(message)),
                _ => Error::Query(format!("Couldn’t save the changes:\n{message}\nNothing was saved.")),
            }),
            Err(e) => Err(e),
        }
    }

    async fn cancel(&self) {
        self.cancel.notify_waiters();
    }
}

/// The last result set of a script (at most `max_rows` rows kept), or the changes of its last
/// statement, like the SQLite driver.
/// `results` end with the script's `select changes()`.
fn script_result(mut results: Vec<StmtResult>, max_rows: Option<u32>) -> QueryResult {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let changes = results.pop().and_then(|r| r.rows.first()?.first()?.as_i64()).map(|n| n.max(0) as u64);
    let affected = changes.or(results.last().map(|r| r.affected_row_count));
    let Some(last) = results.into_iter().rev().find(|r| !r.cols.is_empty()) else {
        return QueryResult { rows_affected: affected, ..Default::default() };
    };
    let columns = last
        .cols
        .iter()
        .map(|c| ColumnInfo { name: c.name.clone().unwrap_or_default(), type_name: String::new(), is_primary_key: false, is_nullable: true })
        .collect();
    let total = last.rows.len();
    let rows = last.rows.iter().take(max_rows).map(|row| row.iter().map(|v| v.decode("")).collect()).collect();
    let truncated = total > max_rows;
    QueryResult { columns, rows, truncated, total_count: truncated.then_some(total as u64), ..Default::default() }
}

/// An error at byte `at` of the script, with its line and column.
fn located(error: Error, script: &str, at: usize) -> Error {
    match error {
        Error::Query(message) => {
            let chars = script.get(..at.min(script.len())).map_or(0, |s| s.chars().count());
            let (line, column) = line_column(script, chars);
            Error::Query(format!("{message} (line {line}, column {column})"))
        }
        other => other,
    }
}

impl From<SqlFailure> for Error {
    fn from(failure: SqlFailure) -> Self {
        failure.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_quoted_columns() {
        let columns = [ColumnInfo { name: "a\"b".into(), type_name: String::new(), is_primary_key: false, is_nullable: true }];
        assert_eq!(quoted_columns(&columns, &["rowid".into()]), "quote(\"a\"\"b\"), quote(rowid)");
    }

    #[test]
    fn writes_cursor_literals() {
        assert_eq!(literal(&CursorValue::Int(-3)), "-3");
        assert_eq!(literal(&CursorValue::float(1.0)), "1.0");
        assert_eq!(literal(&CursorValue::float(f64::INFINITY)), "9e999");
        assert_eq!(literal(&CursorValue::Text("it's".into())), "'it''s'");
        assert_eq!(literal(&CursorValue::Bytes(vec![0, 255])), "X'00ff'");
        assert_eq!(literal(&CursorValue::Null), "NULL");
    }

    #[test]
    fn reads_edit_checks() {
        assert_eq!(failed_check("ERROR: bad JSON path: 'dbear-edit:3:0'"), Some((3, 0)));
        assert_eq!(failed_check("ERROR: bad JSON path: 'dbear-edit:0:2'"), Some((0, 2)));
        assert_eq!(failed_check("ERROR: UNIQUE constraint failed: t.id"), None);
    }

    #[test]
    fn keeps_the_last_result_set() {
        let rows = |n: usize| StmtResult {
            cols: vec![crate::libsql::hrana::Col { name: Some("x".into()), decltype: None }],
            rows: (0..n).map(|i| vec![HValue::Integer { value: i.to_string() }]).collect(),
            affected_row_count: 0,
        };
        let changes = |n: i64| StmtResult { rows: vec![vec![HValue::Integer { value: n.to_string() }]], ..Default::default() };
        let dml = StmtResult { affected_row_count: 6, ..Default::default() };
        let result = script_result(vec![rows(1), rows(5), changes(0)], Some(2));
        assert_eq!((result.rows.len(), result.truncated, result.total_count), (2, true, Some(5)));
        assert_eq!(script_result(vec![rows(1), dml, changes(4)], None).rows.len(), 1);
        let only_changes = script_result(vec![StmtResult { affected_row_count: 6, ..Default::default() }, changes(4)], None);
        assert_eq!((only_changes.rows_affected, only_changes.columns.len()), (Some(4), 0));
    }

    #[test]
    fn locates_errors() {
        let script = "\n  selec 1";
        assert_eq!(located(Error::Query("ERROR: x".into()), script, 3), Error::Query("ERROR: x (line 2, column 3)".into()));
    }
}
