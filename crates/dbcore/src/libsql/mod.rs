//! Turso / libSQL driver: remote databases (`libsql://…`, `sqld`) over Hrana 3 HTTP.
//!
//! The SQL is SQLite's, so quoting, catalog queries, decoding and edits follow the SQLite driver.
//! What differs:
//! - Every call opens its own short-lived Hrana stream and closes it in the same (or the last)
//!   request, so an abandoned stream never holds a transaction or its write lock. A transaction a
//!   script leaves open is rolled back when the script ends.
//! - Scripts run as one batch through a cursor, so rows stream in and past `max_rows` they're only
//!   counted.
//! - No row counts (`total_count`, `estimated_row_count`): Turso bills rows read and `count(*)`
//!   reads the whole table.
//! - Keyset paging (`fetch_page`) uses the SQLite driver's order (`rowid` or the primary key as
//!   tiebreak), with cursor values sent back as typed Hrana arguments.
//! - `cancel` drops the request, which stops a streaming cursor on the server, but Hrana can't
//!   interrupt a statement that's still running there.
//!
//! Local libSQL files are SQLite files and use the SQLite driver.

mod catalog;
pub(crate) mod hrana;
mod script;
pub(crate) mod url;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::dialect::line_column;
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{page_sql, CursorValue, PageCursor, RowPage, Start};
use crate::model::*;
use catalog::SQLITE;
use hrana::{Arg, HValue, Batch, BatchResult, Client, CursorEntry, HranaError, Stmt, StmtResult, Stream, StreamRequest, StreamResponse, StreamResult};

pub struct LibsqlDriver {
    config: ConnectionConfig,
    /// Created on first use (it can fail, e.g. TLS setup); dropped by `disconnect`.
    client: Mutex<Option<Arc<Client>>>,
    connected: AtomicBool,
    /// Fired by `cancel`; the running `execute` stops waiting and returns `Cancelled`.
    cancel: Notify,
}

impl LibsqlDriver {
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

    /// Notes whether the server answered, for `is_connected`.
    fn track<T>(&self, result: Result<T>) -> Result<T> {
        match &result {
            Ok(_) => self.connected.store(true, Ordering::Relaxed),
            Err(Error::ConnectionFailed(_)) => self.connected.store(false, Ordering::Relaxed),
            Err(_) => {}
        }
        result
    }

    /// Runs `statements` on a fresh stream that's closed in the same request: one round trip.
    /// Returns each statement's result or error.
    async fn run(&self, statements: impl IntoIterator<Item = Stmt>) -> Result<Vec<Result<StmtResult>>> {
        let result = async {
            let mut requests: Vec<StreamRequest> = statements.into_iter().map(|stmt| StreamRequest::Execute { stmt }).collect();
            requests.push(StreamRequest::Close);
            let mut results = self.client()?.stream().pipeline(&requests).await?;
            results.pop();
            Ok(results.into_iter().map(execute_result).collect::<Vec<_>>())
        }
        .await;
        self.track(result)
    }

    async fn run_one(&self, statement: Stmt) -> Result<StmtResult> {
        self.run([statement]).await?.pop().expect("one result per statement")
    }

    async fn meta(&self, table: &TableInfo) -> Result<catalog::TableMeta> {
        let [master, info] = take2(self.run(catalog::table_meta_statements(table)).await?);
        catalog::table_meta(table, master?, info?)
    }
}

fn execute_result(result: StreamResult) -> Result<StmtResult> {
    match result {
        StreamResult::Ok { response: StreamResponse::Execute { result } } => Ok(result),
        StreamResult::Ok { .. } => Err(Error::Internal("unexpected response type".into())),
        StreamResult::Error { error } => Err(error.into_error()),
    }
}

fn batch_result(result: StreamResult) -> Result<BatchResult> {
    match result {
        StreamResult::Ok { response: StreamResponse::Batch { result } } => Ok(result),
        StreamResult::Ok { .. } => Err(Error::Internal("unexpected response type".into())),
        StreamResult::Error { error } => Err(error.into_error()),
    }
}

fn take2<T>(v: Vec<T>) -> [T; 2] {
    v.try_into().unwrap_or_else(|_| unreachable!("two statements, two results"))
}

/// Closes the stream when dropped (in the background), even if the caller's future was dropped
/// mid-request: the server then rolls back whatever the stream had open instead of holding it
/// until it expires.
struct StreamGuard(Option<Stream>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take().filter(Stream::is_open) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(stream.close());
            }
        }
    }
}

#[async_trait]
impl Driver for LibsqlDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.run_one(Stmt::new("select 1")).await.map(|_| ())
    }

    async fn disconnect(&self) {
        self.cancel().await;
        self.client.lock().unwrap_or_else(|p| p.into_inner()).take();
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        Ok(catalog::schemas(self.run_one(catalog::list_tables()).await?))
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        Ok(catalog::columns(self.run_one(catalog::list_columns()).await?))
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        let meta = self.meta(table).await?;
        let relation = SQLITE.quote_relation(&table.schema, &table.name);
        // Same order as `fetch_page`, so OFFSET and keyset pages agree.
        let keyset = catalog::keyset_for(&meta, table, query)?;
        let sql = page_sql(&relation, &[], query.filter.as_deref(), None, &keyset.order_by(), u64::from(limit), offset);
        // Errors leave out positions: they'd point into the generated query, not at the user's filter.
        let page = self.run_one(Stmt::new(sql)).await?;
        // `total_count` stays `None`, even on the first page: Turso bills every row read, and
        // `count(*)` reads the whole table (or every match of the filter). The grid pages until a
        // short page instead.
        Ok(QueryResult { rows: catalog::decode_rows(&page, &meta.columns), columns: meta.columns, ..Default::default() })
    }

    /// Like `fetch_rows`, but seeks past the cursor's last row (sent back as arguments) when the
    /// table has a usable row id or key, as the SQLite driver does. Never counts rows (see above).
    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let meta = self.meta(table).await?;
        let relation = SQLITE.quote_relation(&table.schema, &table.name);
        let keyset = catalog::keyset_for(&meta, table, query)?;
        let filter = query.filter.as_deref();
        let width = meta.columns.len();
        let extra: Vec<String> = keyset.columns.iter().filter(|c| c.index >= width).map(|c| c.expr.clone()).collect();

        // `?N` is the N-th sort term, however often the predicate repeats it.
        let mut args: Vec<Arg> = Vec::new();
        let start = keyset.start(after, |i, v| match v {
            // JSON has no infinities; SQLite reads these literals as ±Inf.
            CursorValue::Float(bits) if !f64::from_bits(*bits).is_finite() => {
                if f64::from_bits(*bits) > 0.0 { "9e999".into() } else { "-9e999".into() }
            }
            v => {
                if args.len() <= i {
                    args.resize(i + 1, Arg::Null);
                }
                args[i] = Arg::from_cursor(v);
                format!("?{}", i + 1)
            }
        });
        let (segments, offset) = match start {
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
        for seek in segments {
            let need = want.saturating_sub(rows.len()) as u64;
            if need == 0 {
                break;
            }
            // The server wants exactly as many arguments as the statement's highest `?N`, and a
            // segment may not use every key: mention the last one in an always-true term.
            let seek = seek.map(|s| if args.is_empty() { s } else { format!("({s}) and ?{n} is ?{n}", n = args.len()) });
            let sql = page_sql(&relation, &extra, filter, seek.as_deref(), &keyset.order_by(), need, offset);
            let args = if seek.is_some() { args.clone() } else { Vec::new() };
            let part = self.run_one(Stmt { args, ..Stmt::new(sql) }).await?;
            keys.extend(part.rows.iter().map(|row| key_indexes.iter().map(|&i| row.get(i).map_or(CursorValue::Null, HValue::to_cursor)).collect()));
            rows.extend(catalog::decode_rows(&part, &meta.columns));
        }
        let result = QueryResult { rows, columns: meta.columns, ..Default::default() };
        Ok(keyset.finish(result, keys, width, limit, after))
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        let [master, info] = catalog::table_meta_statements(table);
        let [indexes, foreign_keys, ddl, referenced_by] = catalog::structure_statements(table);
        let mut results = self.run([master, info, indexes, foreign_keys, ddl, referenced_by]).await?.into_iter();
        let mut next = || results.next().expect("one result per statement");
        let (master, info) = (next()?, next()?);
        let meta = catalog::table_meta(table, master, info)?;
        // Optional parts: a server without one of these pragmas still shows the columns.
        let (indexes, foreign_keys, ddl, referenced_by) = (next().ok(), next().ok(), next().ok(), next().ok());
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

        let mut guard = StreamGuard(Some(client.stream()));
        let stream = guard.0.as_mut().expect("set above");
        let result = tokio::select! {
            result = run_script(stream, sql, &statements, max_rows) => result,
            _ = &mut cancelled => Err(Error::Cancelled),
        };
        drop(guard);
        self.track(result)
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let result = async {
            let mut guard = StreamGuard(Some(self.client()?.stream()));
            let stream = guard.0.as_mut().expect("set above");
            // `immediate` takes the write lock now, so a busy database fails before anything runs.
            let steps = std::iter::once(Stmt::new("begin immediate")).chain(statements.iter().map(|s| Stmt::new(&s.sql)));
            let batch = Batch::chained(steps);
            let mut results = stream.pipeline(&[StreamRequest::Batch { batch }]).await?;
            let batch = batch_result(results.pop().expect("one result"))?;
            if let Some(Some(error)) = batch.step_errors.first() {
                return Err(error.clone().into_error());
            }
            let outcome = check_steps(statements, &batch);

            let finish = if outcome.is_ok() { "commit" } else { "rollback" };
            let mut results = stream.pipeline(&[StreamRequest::Execute { stmt: Stmt::new(finish) }, StreamRequest::Close]).await?;
            results.pop();
            let total = outcome?;
            execute_result(results.pop().expect("one result"))?;
            Ok(total)
        }
        .await;
        self.track(result)
    }

    async fn cancel(&self) {
        self.cancel.notify_waiters();
    }
}

/// Every edit statement ran and touched the rows it should (see [`edit::check_affected`]).
/// Step 0 is the `begin`.
fn check_steps(statements: &[EditStatement], batch: &BatchResult) -> Result<u64> {
    let mut total = 0;
    for (i, statement) in statements.iter().enumerate() {
        let step = i + 1;
        if let Some(Some(error)) = batch.step_errors.get(step) {
            return Err(edit::failed(statement, error.clone().into_error()));
        }
        let Some(Some(result)) = batch.step_results.get(step) else {
            return Err(Error::Internal(format!("no result for {}", statement.sql)));
        };
        edit::check_affected(statement, result.affected_row_count)?;
        total += result.affected_row_count;
    }
    Ok(total)
}

/// Streams a script's batch; returns the last result set, or the changes of the last statement
/// (like the SQLite driver).
async fn run_script(stream: &mut Stream, sql: &str, statements: &[script::Statement<'_>], max_rows: Option<u32>) -> Result<QueryResult> {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let batch = Batch::chained(statements.iter().map(|s| Stmt::new(s.sql)));
    let mut cursor = stream.cursor(&batch).await?;

    let mut last_rows: Option<QueryResult> = None;
    let mut last_affected: Option<u64> = None;
    let mut current: Option<QueryResult> = None;
    let mut types: Vec<String> = Vec::new();
    while let Some(entry) = cursor.next().await? {
        match entry {
            CursorEntry::StepBegin { cols, .. } => {
                types = cols.iter().map(|c| c.decltype.clone().unwrap_or_default().to_lowercase()).collect();
                let columns: Vec<ColumnInfo> = cols
                    .into_iter()
                    .zip(&types)
                    .map(|(c, t)| ColumnInfo { name: c.name.unwrap_or_default(), type_name: t.clone(), is_primary_key: false, is_nullable: true })
                    .collect();
                current = (!columns.is_empty()).then(|| QueryResult { columns, ..Default::default() });
            }
            CursorEntry::Row { row } => {
                if let Some(result) = current.as_mut() {
                    if result.rows.len() < max_rows {
                        result.rows.push(row.iter().enumerate().map(|(i, v)| v.decode(types.get(i).map_or("", String::as_str))).collect());
                    } else {
                        result.truncated = true;
                        *result.total_count.get_or_insert(max_rows as u64) += 1;
                    }
                }
            }
            CursorEntry::StepEnd { affected_row_count } => match current.take() {
                Some(result) => last_rows = Some(result),
                None => last_affected = Some(affected_row_count),
            },
            CursorEntry::StepError { step, error } => {
                let start = statements.get(step as usize).map_or(0, |s| s.start);
                return Err(located(error, sql, start));
            }
            CursorEntry::Error { error } => return Err(error.into_error()),
            CursorEntry::Other => {}
        }
    }
    Ok(last_rows.unwrap_or(QueryResult { rows_affected: last_affected, ..Default::default() }))
}

/// A statement's error, pointing at where that statement starts in the script (Hrana errors carry
/// no offset of their own).
fn located(error: HranaError, script: &str, start: usize) -> Error {
    if error.is_interrupt() {
        return Error::Cancelled;
    }
    let chars = script.get(..start).map_or(0, |s| s.chars().count());
    let (line, column) = line_column(script, chars);
    Error::Query(format!("{} (line {line}, column {column})", error.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_statement_errors() {
        let script = "select 1;\n  selec 2;";
        let start = script::split(script)[1].start;
        let error = HranaError { message: "SQLite error: near \"selec\": syntax error".into(), code: None };
        assert_eq!(located(error, script, start), Error::Query("ERROR: near \"selec\": syntax error (line 2, column 3)".into()));
    }

    #[test]
    fn checks_edit_steps() {
        let update = EditStatement { sql: "update t set a = 1 where id = 1;".into(), expect_one_row: true, target: "\"id\" = 1".into() };
        let ok = |n| Some(StmtResult { affected_row_count: n, ..Default::default() });
        let batch = BatchResult { step_results: vec![ok(0), ok(1)], step_errors: vec![None, None] };
        assert_eq!(check_steps(std::slice::from_ref(&update), &batch), Ok(1));
        let missing = BatchResult { step_results: vec![ok(0), ok(0)], step_errors: vec![None, None] };
        assert!(check_steps(std::slice::from_ref(&update), &missing).is_err());
        let failed = BatchResult {
            step_results: vec![ok(0), None],
            step_errors: vec![None, Some(HranaError { message: "constraint failed".into(), code: None })],
        };
        let Err(Error::Query(message)) = check_steps(&[update], &failed) else { panic!() };
        assert!(message.contains("constraint failed") && message.contains("Nothing was saved"), "{message}");
    }
}
