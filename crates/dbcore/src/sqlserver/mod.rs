//! Microsoft SQL Server / Azure SQL driver (tiberius over TDS, rustls on ring).
//!
//! Like Postgres, a connection browses one database (`ConnectionConfig::database`, `master` when
//! empty); its schemas (`dbo`, …) are the sections, and other databases are opened as their own
//! connection. Tables are addressed as `[schema].[table]`.
//!
//! Scripts are split on `GO` lines (a client command) and each batch is sent as one SQL batch, so
//! session state (temp tables, `SET`, `USE`, open transactions) carries over between runs.
//!
//! tiberius is vendored with a small patch (see `vendor/README.md`): ring instead of aws-lc-rs,
//! DONE row counts on query streams, exact MONEY values.

pub(crate) mod decode;
mod describe;
mod paging;
pub mod script;

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, EncryptionLevel, QueryItem, SqlBrowser};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::dialect::{error_chain, Dialect};
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{CursorValue, Keyset, PageCursor, RowPage, SeekColumn, Start};
use crate::model::*;

use self::script::{check_filter, format_error, split_batches};

pub(crate) type Client = tiberius::Client<Compat<TcpStream>>;

const MSSQL: Dialect = Dialect(DatabaseKind::SqlServer);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle sessions are checked before reuse: servers and proxies drop idle connections.
const PING_AFTER_IDLE: Duration = Duration::from_secs(60);
/// How long a cancelled batch may take to acknowledge the attention before the connection is dropped.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// Tables smaller than this (by `sys.partitions`) get an exact `count_big(*)`.
const EXACT_COUNT_THRESHOLD: u64 = 100_000;
/// Hidden from the database list unless it's the one that's open.
const SYSTEM_DATABASES: &[&str] = &["master", "model", "msdb", "tempdb"];
/// `sys`, `INFORMATION_SCHEMA`, `guest` and the fixed role schemas (`db_owner`… have ids ≥ 16384).
const USER_SCHEMAS: &str = "s.schema_id < 16384 and s.name not in (N'sys', N'INFORMATION_SCHEMA', N'guest')";

pub struct SqlServerDriver {
    config: ConnectionConfig,
    /// Catalog and table browsing.
    browse: Session,
    /// User scripts: a long query doesn't block browsing, and `cancel` only hits scripts.
    query: Session,
    /// Saving row edits, in a transaction of its own.
    edit: Session,
    cancel: CancelSignal,
}

/// Wakes a running script so it can send a TDS attention.
#[derive(Default)]
struct CancelSignal {
    requested: AtomicBool,
    notify: Notify,
}

impl CancelSignal {
    fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

async fn wait_for(signal: Option<&CancelSignal>) {
    match signal {
        Some(signal) => signal.wait().await,
        None => std::future::pending().await,
    }
}

struct Conn {
    client: Client,
    used: Instant,
    /// A request is in flight: if the lease is dropped now (its future cancelled), the connection
    /// is mid-response and can't be reused.
    busy: bool,
}

/// One lazily (re)connected server connection; tiberius needs `&mut Client`, so it's leased.
struct Session {
    conn: Mutex<Option<Conn>>,
    open: AtomicBool,
    /// Run once after connecting.
    setup: &'static str,
}

impl Session {
    fn new(setup: &'static str) -> Self {
        Self { conn: Mutex::new(None), open: AtomicBool::new(false), setup }
    }

    async fn lease(&self, config: &ConnectionConfig) -> Result<Lease<'_>> {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_mut() {
            if conn.used.elapsed() > PING_AFTER_IDLE {
                let alive = async { conn.client.simple_query("select 1").await?.into_results().await };
                if tokio::time::timeout(Duration::from_secs(5), alive).await.map_or(true, |r| r.is_err()) {
                    guard.take();
                }
            }
        }
        if guard.is_none() {
            let client = connect(config, self.setup).await?;
            *guard = Some(Conn { client, used: Instant::now(), busy: false });
        }
        self.open.store(true, Ordering::Relaxed);
        Ok(Lease { guard, session: self })
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    async fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
        if let Some(conn) = self.conn.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.client.close()).await;
        }
    }
}

/// A locked, open session connection.
struct Lease<'a> {
    guard: MutexGuard<'a, Option<Conn>>,
    session: &'a Session,
}

impl Lease<'_> {
    /// The client, marked busy until [`Lease::check`] sees how the request ended.
    fn client(&mut self) -> &mut Client {
        let conn = self.guard.as_mut().expect("leased sessions are open");
        conn.busy = true;
        &mut conn.client
    }

    /// Keeps the connection after a complete response (server errors included), drops it after
    /// network or protocol errors, so the next call reconnects.
    fn check<T>(&mut self, result: std::result::Result<T, TdsError>) -> std::result::Result<T, TdsError> {
        match &result {
            Ok(_) | Err(TdsError::Server(_)) => self.done(),
            Err(_) => self.discard(),
        }
        result
    }

    fn done(&mut self) {
        if let Some(conn) = self.guard.as_mut() {
            conn.busy = false;
        }
    }

    fn discard(&mut self) {
        self.guard.take();
        self.session.open.store(false, Ordering::Relaxed);
    }

    /// Runs a catalog query and returns its first result set as values.
    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Value>>> {
        Ok(self.results(sql).await?.into_iter().next().unwrap_or_default())
    }

    /// Every result set of a batch, as values.
    async fn results(&mut self, sql: &str) -> Result<Vec<Vec<Vec<Value>>>> {
        let client = self.client();
        let result = async { client.simple_query(sql).await?.into_results().await }.await;
        let sets = self.check(result).map_err(|e| query_error(&e, 1))?;
        Ok(sets.into_iter().map(|rows| rows.iter().map(|r| r.cells().map(|(_, d)| decode::value(d)).collect()).collect()).collect())
    }

    /// Runs a page query: its rows, and the values at `key_indexes` of each row (for cursors).
    async fn page(&mut self, sql: &str, key_indexes: &[usize]) -> Result<(QueryResult, Vec<Vec<CursorValue>>)> {
        match run_batch(self.client(), sql, None, None, key_indexes).await {
            Ok(ran) => {
                self.done();
                Ok((ran.rows.unwrap_or_default(), ran.keys))
            }
            Err(Run::Cancelled) => unreachable!("no cancel signal"),
            Err(Run::Failed(e)) => Err(query_error(&self.check(Err::<(), _>(e)).unwrap_err(), 1)),
        }
    }

    /// Runs a batch without result rows; returns the last row count it reported.
    async fn exec(&mut self, sql: &str) -> Result<Option<u64>> {
        let result = run_batch(self.client(), sql, Some(0), None, &[]).await;
        match result {
            Ok(ran) => {
                self.done();
                Ok(ran.affected)
            }
            Err(Run::Cancelled) => unreachable!("no cancel signal"),
            Err(Run::Failed(e)) => Err(query_error(&self.check(Err::<(), _>(e)).unwrap_err(), 1)),
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        match self.guard.as_mut() {
            Some(conn) if conn.busy => self.discard(),
            Some(conn) => conn.used = Instant::now(),
            None => {}
        }
    }
}

impl SqlServerDriver {
    pub fn new(config: ConnectionConfig) -> Self {
        Self {
            config,
            browse: Session::new(""),
            query: Session::new("set nocount off"),
            // All or nothing; matched-row counts reported; `'2024-01-02 …'` read as y-m-d whatever the login's language.
            edit: Session::new("set xact_abort on; set nocount off; set dateformat ymd"),
            cancel: CancelSignal::default(),
        }
    }
}

// MARK: Connecting

pub(crate) async fn connect(config: &ConnectionConfig, setup: &str) -> Result<Client> {
    let attempt = |level: EncryptionLevel| async move {
        match tokio::time::timeout(CONNECT_TIMEOUT, open(config, level)).await {
            Ok(result) => result,
            Err(_) => Err(TdsError::Io { kind: std::io::ErrorKind::TimedOut, message: "timed out".into() }),
        }
    };
    let result = match config.ssl_mode {
        // Only the login is encrypted (JDBC `encrypt=false`); a server that forces encryption upgrades it.
        SslMode::Disable => attempt(EncryptionLevel::Off).await,
        // Encrypt everything if the server can; very old or stripped-down servers can't.
        SslMode::Prefer => match attempt(EncryptionLevel::On).await {
            Err(e) if !matches!(e, TdsError::Server(_)) && e.to_string().to_ascii_lowercase().contains("encrypt") => {
                attempt(EncryptionLevel::NotSupported).await.map_err(|_| e)
            }
            other => other,
        },
        SslMode::Require | SslMode::VerifyFull => attempt(EncryptionLevel::Required).await,
    };
    let mut client = result.map_err(|e| Error::ConnectionFailed(connect_error(&e)))?;
    if !setup.is_empty() {
        let result = async { client.simple_query(setup).await?.into_results().await }.await;
        result.map_err(|e| Error::ConnectionFailed(connect_error(&e)))?;
    }
    Ok(client)
}

async fn open(config: &ConnectionConfig, level: EncryptionLevel) -> std::result::Result<Client, TdsError> {
    let mut tds = tiberius::Config::new();
    let host = config.host.trim();
    // `host\INSTANCE`: a named instance, found through SQL Server Browser unless a port is given.
    let (host, instance) = match host.split_once('\\') {
        Some((host, instance)) => (host, Some(instance)),
        None => (host, None),
    };
    tds.host(host);
    match (config.port, instance) {
        (Some(port), _) => tds.port(port),
        (None, Some(instance)) => tds.instance_name(instance),
        (None, None) => tds.port(1433),
    }
    tds.database(config.default_database());
    tds.application_name("dbear");
    let user = config.user.as_deref().filter(|u| !u.is_empty()).unwrap_or("sa");
    tds.authentication(AuthMethod::sql_server(user, config.password.as_deref().unwrap_or_default()));
    tds.encryption(level);
    if config.ssl_mode != SslMode::VerifyFull {
        // Encrypt without verifying (self-signed certificates are the norm), like libpq's `require`.
        tds.trust_cert();
    }
    // Through an SSH tunnel: connect to its local end; `host` still names the server for TLS.
    // (Named instances need a port then: SQL Server Browser is UDP, which the tunnel doesn't carry.)
    if let Some(port) = config.tunneled_port() {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        tcp.set_nodelay(true)?;
        return match tiberius::Client::connect(tds, tcp.compat_write()).await {
            Err(TdsError::Routing { host, port }) => Err(TdsError::Io {
                kind: std::io::ErrorKind::Unsupported,
                message: format!("the server redirected to {host}:{port}, which the SSH tunnel doesn’t reach"),
            }),
            other => other,
        };
    }
    // Azure SQL's gateway may redirect to the node that hosts the database.
    let mut redirected = false;
    loop {
        let tcp = TcpStream::connect_named(&tds).await?;
        tcp.set_nodelay(true)?;
        match tiberius::Client::connect(tds.clone(), tcp.compat_write()).await {
            Err(TdsError::Routing { host, port }) if !redirected => {
                redirected = true;
                tds.host(host);
                tds.port(port);
            }
            other => return other,
        }
    }
}

#[async_trait]
impl Driver for SqlServerDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.browse.lease(&self.config).await.map(drop)
    }

    async fn disconnect(&self) {
        self.cancel().await;
        self.browse.close().await;
        self.query.close().await;
        self.edit.close().await;
    }

    async fn is_connected(&self) -> bool {
        self.browse.is_open() || self.query.is_open()
    }

    /// User databases this login can open, plus the open one. System databases are hidden.
    async fn list_databases(&self) -> Result<Vec<String>> {
        let mut lease = self.browse.lease(&self.config).await?;
        let rows = lease.rows("select name from sys.databases where state = 0 and has_dbaccess(name) = 1").await?;
        let current = self.config.default_database();
        let mut names: Vec<String> = rows
            .into_iter()
            .map(|r| text(&r, 0))
            .filter(|n| n.eq_ignore_ascii_case(current) || !SYSTEM_DATABASES.iter().any(|s| s.eq_ignore_ascii_case(n)))
            .collect();
        if !names.iter().any(|n| n.eq_ignore_ascii_case(current)) {
            names.push(current.to_string());
        }
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        let sql = format!(
            "select s.name, o.name, o.type, p.rows
             from sys.schemas s
             left join sys.objects o on o.schema_id = s.schema_id and o.type in ('U', 'V') and o.is_ms_shipped = 0
             left join (select object_id, sum(rows) as rows from sys.partitions where index_id in (0, 1) group by object_id) p
               on p.object_id = o.object_id
             where {USER_SCHEMAS}
             order by s.name, o.name"
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let rows = lease.rows(&sql).await?;
        let mut schemas: Vec<Schema> = Vec::new();
        for row in rows {
            let schema = text(&row, 0);
            if schemas.last().is_none_or(|s| s.name != schema) {
                schemas.push(Schema { name: schema.clone(), tables: Vec::new() });
            }
            if row[1].is_null() {
                continue;
            }
            let kind = if text(&row, 2).trim() == "V" { TableKind::View } else { TableKind::Table };
            schemas.last_mut().expect("pushed above").tables.push(TableInfo {
                schema,
                name: text(&row, 1),
                kind,
                estimated_row_count: if kind == TableKind::View { None } else { int(&row, 3).map(|n| n.max(0) as u64) },
            });
        }
        Ok(schemas)
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        let filter = format!("o.type in ('U', 'V') and o.is_ms_shipped = 0 and {USER_SCHEMAS}");
        let mut lease = self.browse.lease(&self.config).await?;
        let rows = lease.rows(&describe::columns_query(&filter)).await?;
        let mut tables: Vec<TableColumns> = Vec::new();
        for row in &rows {
            let column = describe::ColumnRow::from(row);
            if tables.last().is_none_or(|t| t.schema != column.schema || t.table != column.table) {
                tables.push(TableColumns { schema: column.schema.clone(), table: column.table.clone(), columns: Vec::new() });
            }
            tables.last_mut().expect("pushed above").columns.push(column.info());
        }
        Ok(tables)
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        if let Some(filter) = query.filter.as_deref() {
            check_filter(filter)?;
        }
        let mut lease = self.browse.lease(&self.config).await?;
        let meta = TableMeta::load(&mut lease, table).await?;
        let relation = MSSQL.quote_relation(&table.schema, &table.name);
        // Same order as `fetch_page`, so OFFSET and keyset pages agree.
        let (keyset, _) = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();
        let sql = paging::page_sql(&relation, filter, None, &keyset.order_by(), u64::from(limit), offset);
        let (mut result, _) = lease.page(&sql, &[]).await?;
        result.columns = meta.columns.clone();
        if offset == 0 {
            result.total_count = meta.total_count(&mut lease, &relation, filter).await?;
        }
        Ok(result)
    }

    /// Seeks past the last row's sort key when the table has a unique NOT NULL key (primary key or
    /// unique index) and every sort column compares the way it sorts; otherwise OFFSET.
    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        if let Some(filter) = query.filter.as_deref() {
            check_filter(filter)?;
        }
        let mut lease = self.browse.lease(&self.config).await?;
        let meta = TableMeta::load(&mut lease, table).await?;
        let relation = MSSQL.quote_relation(&table.schema, &table.name);
        let (keyset, kinds) = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();

        let (segments, offset) = match keyset.start(after, |i, v| paging::render_key(kinds[i], v)) {
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
            let sql = paging::page_sql(&relation, filter, seek.as_deref(), &keyset.order_by(), need, offset);
            let (part, part_keys) = lease.page(&sql, &keyset.key_indexes()).await?;
            result.rows.extend(part.rows);
            keys.extend(part_keys);
        }
        result.columns = meta.columns.clone();
        if after.is_none() {
            result.total_count = meta.total_count(&mut lease, &relation, filter).await?;
        }
        Ok(keyset.finish(result, keys, meta.columns.len(), limit, after))
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        let mut lease = self.browse.lease(&self.config).await?;
        // Fails with "table not found" before the catalog queries quietly return nothing.
        let meta = TableMeta::load(&mut lease, table).await?;
        describe::describe(&mut lease, table, meta.is_view).await
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        let batches = split_batches(sql);
        let mut lease = self.query.lease(&self.config).await?;
        self.cancel.requested.store(false, Ordering::SeqCst);
        let mut last_rows: Option<QueryResult> = None;
        let mut last_affected: Option<u64> = None;
        for batch in &batches {
            for _ in 0..batch.repeat {
                if self.cancel.is_requested() {
                    return Err(Error::Cancelled);
                }
                match run_batch(lease.client(), &batch.sql, max_rows, Some(&self.cancel), &[]).await {
                    Ok(ran) => {
                        lease.done();
                        if ran.rows.is_some() {
                            last_rows = ran.rows;
                        }
                        last_affected = ran.affected.or(last_affected);
                    }
                    Err(Run::Cancelled) => {
                        // The request is still running on the server: interrupt it (TDS attention) and
                        // read up to its acknowledgement, so the session can be reused.
                        let attention = tokio::time::timeout(CANCEL_TIMEOUT, lease.client().cancel_query()).await;
                        match attention {
                            Ok(Ok(())) => lease.done(),
                            _ => lease.discard(),
                        }
                        return Err(Error::Cancelled);
                    }
                    Err(Run::Failed(e)) => {
                        let e = lease.check(Err::<(), _>(e)).unwrap_err();
                        return Err(query_error(&e, batch.first_line));
                    }
                }
            }
        }
        Ok(last_rows.unwrap_or(QueryResult { rows_affected: last_affected, ..Default::default() }))
    }

    async fn cancel(&self) {
        self.cancel.request();
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let mut lease = self.edit.lease(&self.config).await?;
        // Roll back first: a save abandoned midway (its future dropped) must never be committed later.
        lease.exec("if @@trancount > 0 rollback; begin transaction").await?;
        let mut total = 0;
        for statement in statements {
            let result = match lease.exec(&statement.sql).await {
                // Triggers report their own counts first; the statement's is the last one.
                Ok(affected) => {
                    let affected = affected.unwrap_or(0);
                    edit::check_affected(statement, affected).map(|()| affected)
                }
                Err(e) => Err(edit::failed(statement, e)),
            };
            match result {
                Ok(affected) => total += affected,
                Err(e) => {
                    if lease.guard.is_some() {
                        let _ = lease.exec("if @@trancount > 0 rollback").await;
                    }
                    return Err(e);
                }
            }
        }
        if let Err(e) = lease.exec("commit transaction").await {
            if lease.guard.is_some() {
                let _ = lease.exec("if @@trancount > 0 rollback").await;
            }
            return Err(match e {
                Error::Query(m) => Error::Query(format!("Couldn’t save:\n{m}\nNothing was saved.")),
                other => other,
            });
        }
        Ok(total)
    }
}

// MARK: Running SQL

struct Ran {
    /// The last result set, if any statement returned rows.
    rows: Option<QueryResult>,
    /// The last row count the server reported (DONE tokens).
    affected: Option<u64>,
    /// Sort-key values of each kept row of the last result set (`key_indexes` of `run_batch`).
    keys: Vec<Vec<CursorValue>>,
}

enum Run {
    Cancelled,
    Failed(TdsError),
}

impl From<TdsError> for Run {
    fn from(e: TdsError) -> Self {
        Run::Failed(e)
    }
}

/// Races `future` against a cancel request.
async fn or_cancel<T>(signal: Option<&CancelSignal>, future: impl Future<Output = std::result::Result<T, TdsError>>) -> std::result::Result<T, Run> {
    tokio::select! {
        biased;
        () = wait_for(signal) => Err(Run::Cancelled),
        result = future => result.map_err(Run::Failed),
    }
}

/// Runs one SQL batch. Rows past `max_rows` are counted, not kept; the stream is drained so later
/// statements still run. A server error anywhere in the batch fails it. The wire values at
/// `key_indexes` are kept too, exactly, for keyset cursors.
async fn run_batch(
    client: &mut Client,
    sql: &str,
    max_rows: Option<u32>,
    cancel: Option<&CancelSignal>,
    key_indexes: &[usize],
) -> std::result::Result<Ran, Run> {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let mut stream = or_cancel(cancel, client.simple_query(sql)).await?;
    let mut current: Option<QueryResult> = None;
    let mut last_rows: Option<QueryResult> = None;
    let (mut keys, mut last_keys) = (Vec::new(), Vec::new());
    while let Some(item) = or_cancel(cancel, stream.try_next()).await? {
        match item {
            QueryItem::Metadata(meta) => {
                if let Some(done) = current.take() {
                    last_rows = Some(done);
                    last_keys = std::mem::take(&mut keys);
                }
                current = Some(QueryResult {
                    columns: meta
                        .columns()
                        .iter()
                        .map(|c| ColumnInfo {
                            name: if c.name().is_empty() { "(No column name)".into() } else { c.name().into() },
                            type_name: decode::type_name(c.column_type()).into(),
                            is_primary_key: false,
                            is_nullable: true,
                        })
                        .collect(),
                    ..Default::default()
                });
            }
            QueryItem::Row(row) => {
                let Some(result) = current.as_mut() else { continue };
                if result.rows.len() < max_rows {
                    if !key_indexes.is_empty() {
                        let cells: Vec<_> = row.cells().map(|(_, data)| data).collect();
                        keys.push(key_indexes.iter().map(|&i| cells.get(i).map_or(CursorValue::Null, |d| paging::key_value(d))).collect());
                    }
                    result.rows.push(row.cells().map(|(_, data)| decode::value(data)).collect());
                } else {
                    result.truncated = true;
                    *result.total_count.get_or_insert(max_rows as u64) += 1;
                }
            }
        }
    }
    let affected = stream.rows_affected().last().copied();
    let (rows, keys) = match current {
        Some(current) => (Some(current), keys),
        None => (last_rows, last_keys),
    };
    Ok(Ran { rows, affected, keys })
}

// MARK: Table metadata

struct TableMeta {
    columns: Vec<ColumnInfo>,
    /// Columns appended to every sort so pages are stable: the primary key, else a unique index on
    /// NOT NULL columns, else nothing (`order by (select null)`, OFFSET paging only).
    key: Vec<String>,
    /// `sys.partitions` row count for tables; `None` for views.
    estimated_rows: Option<u64>,
    is_view: bool,
}

impl TableMeta {
    async fn load(lease: &mut Lease<'_>, table: &TableInfo) -> Result<Self> {
        let object = object_id(table);
        let sql = format!(
            "select o.type, (select sum(p.rows) from sys.partitions p where p.object_id = o.object_id and p.index_id in (0, 1))
             from sys.objects o where o.object_id = {object} and o.type in ('U', 'V');
             {columns};
             select i.index_id, c.name, c.is_nullable
             from sys.indexes i
             join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id
             join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id
             where i.object_id = {object} and i.is_unique = 1 and i.is_primary_key = 0 and i.has_filter = 0
               and i.is_disabled = 0 and ic.is_included_column = 0
             order by i.index_id, ic.key_ordinal",
            columns = describe::columns_query(&format!("o.object_id = {object}")),
        );
        let mut sets = lease.results(&sql).await?.into_iter();
        let info = sets.next().unwrap_or_default();
        let Some(info) = info.first() else { return Err(Error::TableNotFound(table.qualified_name())) };
        let is_view = text(info, 0).trim() == "V";
        let column_rows: Vec<describe::ColumnRow> = sets.next().unwrap_or_default().iter().map(describe::ColumnRow::from).collect();
        let columns: Vec<ColumnInfo> = column_rows.iter().map(|c| c.info()).collect();

        let mut primary_key: Vec<&describe::ColumnRow> = column_rows.iter().filter(|c| c.pk_ordinal > 0).collect();
        primary_key.sort_by_key(|c| c.pk_ordinal);
        let key = if !primary_key.is_empty() {
            primary_key.iter().map(|c| c.name.clone()).collect()
        } else {
            best_unique_index(&sets.next().unwrap_or_default())
        };
        Ok(Self {
            columns,
            key,
            estimated_rows: if is_view { None } else { Some(int(info, 1).unwrap_or(0).max(0) as u64) },
            is_view,
        })
    }
}

/// The unique index with the fewest columns whose key columns are all NOT NULL
/// (rows: index id, column, is nullable — in key order).
fn best_unique_index(rows: &[Vec<Value>]) -> Vec<String> {
    let mut indexes: Vec<(i64, Vec<String>, bool)> = Vec::new();
    for row in rows {
        let id = int(row, 0).unwrap_or_default();
        if indexes.last().is_none_or(|(i, _, _)| *i != id) {
            indexes.push((id, Vec::new(), false));
        }
        let index = indexes.last_mut().expect("pushed above");
        index.1.push(text(row, 1));
        index.2 |= flag(row, 2);
    }
    indexes.into_iter().filter(|(_, _, nullable)| !nullable).min_by_key(|(id, cols, _)| (cols.len(), *id)).map(|(_, c, _)| c).unwrap_or_default()
}

impl TableMeta {
    /// Page order: the user's sort, then the key. Seeking needs the key and sort types whose
    /// comparison agrees with their order (see [`paging::key_kind`]); also returns how to spell
    /// each term's values.
    fn keyset(&self, table: &TableInfo, query: &RowQuery) -> Result<(Keyset, Vec<paging::KeyKind>)> {
        let tiebreak: Vec<SeekColumn> = self.key.iter().filter_map(|c| SeekColumn::column(MSSQL, &self.columns, c)).collect();
        let mut keyset = Keyset::new(MSSQL, table, query, &self.columns, tiebreak, !self.key.is_empty())?;
        let kinds: Option<Vec<paging::KeyKind>> =
            keyset.columns.iter().map(|c| paging::key_kind(&self.columns[c.index].type_name)).collect();
        keyset.enabled &= kinds.is_some();
        Ok((keyset, kinds.unwrap_or_default()))
    }

    /// Exact for small tables, `sys.partitions` for big unfiltered ones, else unknown (views too).
    async fn total_count(&self, lease: &mut Lease<'_>, relation: &str, filter: Option<&str>) -> Result<Option<u64>> {
        Ok(match self.estimated_rows {
            None => None,
            Some(rows) if rows >= EXACT_COUNT_THRESHOLD && filter.is_some() => None,
            Some(rows) if rows >= EXACT_COUNT_THRESHOLD => Some(rows),
            Some(_) => {
                let where_clause = filter.map_or(String::new(), |f| format!(" where (\n{f}\n)"));
                let rows = lease.rows(&format!("select count_big(*) from {relation}{where_clause}")).await?;
                rows.first().and_then(|r| int(r, 0)).map(|n| n.max(0) as u64)
            }
        })
    }
}

/// `object_id(N'[schema].[table]')`.
fn object_id(table: &TableInfo) -> String {
    format!("object_id({})", MSSQL.quote_literal(&MSSQL.quote_relation(&table.schema, &table.name)))
}

// MARK: Values

fn text(row: &[Value], i: usize) -> String {
    match row.get(i) {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.display(),
    }
}

fn opt_text(row: &[Value], i: usize) -> Option<String> {
    row.get(i).filter(|v| !v.is_null()).map(Value::display)
}

fn int(row: &[Value], i: usize) -> Option<i64> {
    match row.get(i)? {
        Value::Int(n) => Some(*n),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Decimal(s) | Value::Text(s) => s.parse().ok(),
        Value::Float(f) => Some(*f as i64),
        Value::Null => None,
    }
}

fn flag(row: &[Value], i: usize) -> bool {
    int(row, i).unwrap_or(0) != 0
}

// MARK: Errors

/// Server errors as SSMS shows them; everything else as a connection problem.
pub(crate) fn query_error(e: &TdsError, first_line: u32) -> Error {
    match e {
        TdsError::Server(token) => Error::Query(format_error(token, first_line)),
        TdsError::Io { .. } | TdsError::Tls(_) | TdsError::Routing { .. } => Error::ConnectionFailed(error_chain(e)),
        _ => Error::Query(error_chain(e)),
    }
}

/// "Login failed for user 'sa'." rather than the wrapper text.
fn connect_error(e: &TdsError) -> String {
    match e {
        TdsError::Server(token) => token.message().to_string(),
        TdsError::Io { message, .. } => message.clone(),
        TdsError::Tls(message) => tls_error(message),
        other => error_chain(other),
    }
}

/// tiberius explains certificate failures in terms of its own API; say what to do in dbear instead.
fn tls_error(message: &str) -> String {
    const REJECTED: &str = "the server's certificate was rejected during the TLS handshake: ";
    match message.strip_prefix(REJECTED) {
        Some(rest) => {
            let reason = rest.split(". ").next().unwrap_or(rest).trim_end_matches('.');
            format!(
                "The server’s certificate couldn’t be verified ({reason}). SQL Server often uses a self-signed \
                 certificate: choose SSL “Require” to encrypt without verifying it, or trust its CA in the system trust store."
            )
        }
        None => format!("TLS: {message}"),
    }
}
