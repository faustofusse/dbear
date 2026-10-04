//! PostgreSQL driver (tokio-postgres + rustls).
//!
//! Values are fetched in Postgres' text format (`simple_query`), so every type, including
//! extension types, renders exactly like psql shows it. Column types come from `prepare`
//! (or the catalog for table browsing), and are used to turn ints/floats/bools into typed
//! values and NUMERIC into an exact `Value::Decimal`.

pub(crate) mod tls;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{pin_mut, TryStreamExt};
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{Kind, Type};
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::dialect::{error_chain, line_column, Dialect};
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{page_sql, CursorValue, Keyset, PageCursor, RowPage, SeekColumn, Start};
use crate::model::*;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Tables smaller than this (by planner estimate) get an exact `count(*)`.
const EXACT_COUNT_THRESHOLD: f32 = 100_000.0;

pub struct PostgresDriver {
    config: ConnectionConfig,
    /// Schema listing and table browsing.
    browse: Session,
    /// User scripts: a long query here doesn't block browsing, and `cancel` only hits scripts.
    query: Session,
    /// Saving row edits: its transaction never mixes with browsing or a script's own transaction.
    edit: Session,
    /// One save at a time on `edit`.
    applying: Mutex<()>,
}

impl PostgresDriver {
    pub fn new(config: ConnectionConfig) -> Self {
        // Floats exactly (the default on Postgres 12+), so keyset cursors can send them back.
        let browse = Session { init: Some("set extra_float_digits = 3"), ..Session::default() };
        Self { config, browse, query: Session::default(), edit: Session::default(), applying: Mutex::new(()) }
    }

    async fn browse_client(&self) -> Result<Arc<Client>> {
        self.browse.client(&self.config).await
    }
}

/// One lazily (re)connected server connection. Queries on it are pipelined, not serialized here.
#[derive(Default)]
struct Session {
    client: Mutex<Option<Arc<Client>>>,
    /// Run once on each new connection.
    init: Option<&'static str>,
}

impl Session {
    async fn client(&self, config: &ConnectionConfig) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(client.clone());
        }
        let client = connect(config).await?;
        if let Some(init) = self.init {
            client.batch_execute(init).await.map_err(|e| query_error(&e, None))?;
        }
        let client = Arc::new(client);
        *slot = Some(client.clone());
        Ok(client)
    }

    async fn is_open(&self) -> bool {
        self.client.lock().await.as_ref().is_some_and(|c| !c.is_closed())
    }

    /// Token for the current connection, if any (cancel needs no client lock).
    async fn cancel_token(&self) -> Option<CancelToken> {
        self.client.lock().await.as_ref().map(|c| c.cancel_token())
    }

    async fn close(&self) {
        // Dropping the client ends the connection task, which sends Terminate.
        self.client.lock().await.take();
    }
}

pub(crate) async fn connect(config: &ConnectionConfig) -> Result<Client> {
    let mut pg = tokio_postgres::Config::new();
    pg.host(&config.host)
        .port(config.port.unwrap_or(5432))
        .dbname(config.default_database())
        .user(config.user.as_deref().unwrap_or("postgres"))
        .application_name("dbear")
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(match config.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
    if let Some(password) = &config.password {
        pg.password(password);
    }

    let (client, connection) = pg
        .connect(tls::connector(config.ssl_mode)?)
        .await
        .map_err(|e| Error::ConnectionFailed(connect_error(&e)))?;
    tokio::spawn(async move {
        // Ends when the client is dropped or the server goes away; `is_closed` reports the latter.
        let _ = connection.await;
    });
    Ok(client)
}

#[async_trait]
impl Driver for PostgresDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        self.browse_client().await.map(drop)
    }

    async fn disconnect(&self) {
        // A running script holds its own client handle; stop it so the connection really goes away.
        self.cancel().await;
        self.browse.close().await;
        self.query.close().await;
        self.edit.close().await;
    }

    async fn is_connected(&self) -> bool {
        self.browse.is_open().await || self.query.is_open().await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let client = self.browse_client().await?;
        let rows = client
            .query(
                r"
                select datname from pg_database
                where datallowconn and not datistemplate
                  and has_database_privilege(datname, 'CONNECT')
                order by datname
                ",
                &[],
            )
            .await
            .map_err(|e| query_error(&e, None))?;
        let mut names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        // The default database is always listed, even if the catalog hides it from us.
        let default = self.config.default_database().to_string();
        if !names.contains(&default) {
            names.push(default);
            names.sort();
        }
        Ok(names)
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        let client = self.browse_client().await?;
        let rows = client
            .query(
                r"
                select n.nspname, c.relname, c.relkind::text, c.reltuples
                from pg_namespace n
                left join pg_class c
                  on c.relnamespace = n.oid
                 and c.relkind in ('r', 'p', 'v', 'm', 'f')
                 and not c.relispartition
                where n.nspname not in ('information_schema')
                  and n.nspname not like 'pg\_%'
                order by n.nspname, c.relname
                ",
                &[],
            )
            .await
            .map_err(|e| query_error(&e, None))?;

        let mut schemas: Vec<Schema> = Vec::new();
        for row in rows {
            let schema: String = row.get(0);
            if schemas.last().is_none_or(|s| s.name != schema) {
                schemas.push(Schema { name: schema.clone(), tables: Vec::new() });
            }
            let Some(name) = row.get::<_, Option<String>>(1) else { continue };
            let relkind: String = row.get(2);
            let reltuples: f32 = row.get(3);
            let kind = if matches!(relkind.as_str(), "v" | "m") { TableKind::View } else { TableKind::Table };
            schemas.last_mut().unwrap().tables.push(TableInfo {
                schema,
                name,
                kind,
                // -1 means "never analyzed"; views have no estimate.
                estimated_row_count: (relkind != "v" && reltuples >= 0.0).then_some(reltuples as u64),
            });
        }
        Ok(schemas)
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        let client = self.browse_client().await?;
        let rows = client
            .query(
                r"
                select n.nspname, c.relname, a.attname,
                       format_type(a.atttypid, a.atttypmod), a.attnotnull,
                       coalesce(a.attnum = any(i.indkey), false)
                from pg_namespace n
                join pg_class c
                  on c.relnamespace = n.oid
                 and c.relkind in ('r', 'p', 'v', 'm', 'f')
                 and not c.relispartition
                join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
                left join pg_index i on i.indrelid = c.oid and i.indisprimary
                where n.nspname not in ('information_schema')
                  and n.nspname not like 'pg\_%'
                order by n.nspname, c.relname, a.attnum
                ",
                &[],
            )
            .await
            .map_err(|e| query_error(&e, None))?;

        let mut tables: Vec<TableColumns> = Vec::new();
        for row in rows {
            let schema: String = row.get(0);
            let table: String = row.get(1);
            if tables.last().is_none_or(|t| t.schema != schema || t.table != table) {
                tables.push(TableColumns { schema, table, columns: Vec::new() });
            }
            tables.last_mut().unwrap().columns.push(ColumnInfo {
                name: row.get(2),
                type_name: row.get(3),
                is_nullable: !row.get::<_, bool>(4),
                is_primary_key: row.get(5),
            });
        }
        Ok(tables)
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        let client = self.browse_client().await?;
        let relation = quote_relation(&table.schema, &table.name);
        let meta = TableMeta::load(&client, &relation, table).await?;
        let keyset = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();
        let sql = page_sql(&relation, &[], filter, None, &keyset.order_by(), u64::from(limit), offset);

        // Positions would point into the generated query, not at what the user typed: leave them out.
        let (mut result, _) = run_page(&client, &sql, &[]).await?;
        // Catalog names read better than wire type names ("timestamp with time zone" vs "timestamptz").
        result.columns = meta.columns.clone();
        if offset == 0 {
            result.total_count = meta.total_count(&client, &relation, filter).await?;
        }
        Ok(result)
    }

    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let client = self.browse_client().await?;
        let relation = quote_relation(&table.schema, &table.name);
        let meta = TableMeta::load(&client, &relation, table).await?;
        let keyset = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();
        let width = meta.columns.len();
        let extra: Vec<String> = keyset.columns.iter().filter(|c| c.index >= width).map(|c| c.expr.clone()).collect();

        // Untyped literals: Postgres reads them as the column's type (enums, domains, tid…).
        let start = keyset.start(after, |_, v| match v {
            CursorValue::Text(t) => PG.quote_literal(t),
            CursorValue::Int(i) => i.to_string(),
            other => unreachable!("Postgres keys are text, got {other:?}"),
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
            let (part, part_keys) = run_page(&client, &sql, &keyset.key_indexes()).await?;
            result.rows.extend(part.rows);
            keys.extend(part_keys);
        }
        result.columns = meta.columns.clone();
        if after.is_none() {
            result.total_count = meta.total_count(&client, &relation, filter).await?;
        }
        Ok(keyset.finish(result, keys, width, limit, after))
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        let client = self.browse_client().await?;
        let relation = quote_relation(&table.schema, &table.name);
        describe(&client, &relation, table).await
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        let client = self.query.client(&self.config).await?;
        // If this future is dropped (e.g. the caller's task is cancelled), stop the server-side query too.
        let guard = CancelOnDrop::new(client.cancel_token(), self.config.ssl_mode);
        let result = run_script(&client, sql, max_rows).await;
        guard.disarm();
        result
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let _one_at_a_time = self.applying.lock().await;
        let client = self.edit.client(&self.config).await?;
        // `rollback` first: a save abandoned midway (its future dropped) must never be committed later.
        client.batch_execute("rollback; begin").await.map_err(|e| query_error(&e, None))?;
        let mut total = 0;
        for statement in statements {
            let result = match client.execute(statement.sql.as_str(), &[]).await {
                Ok(affected) => edit::check_affected(statement, affected).map(|()| affected),
                Err(e) => Err(edit::failed(statement, query_error(&e, None))),
            };
            match result {
                Ok(affected) => total += affected,
                Err(e) => {
                    let _ = client.batch_execute("rollback").await;
                    return Err(e);
                }
            }
        }
        // Deferred constraints are checked here.
        if let Err(e) = client.batch_execute("commit").await {
            let _ = client.batch_execute("rollback").await;
            return Err(match query_error(&e, None) {
                Error::Query(m) => Error::Query(format!("Couldn’t save:\n{m}\nNothing was saved.")),
                other => other,
            });
        }
        Ok(total)
    }

    async fn cancel(&self) {
        if let Some(token) = self.query.cancel_token().await {
            let _ = send_cancel(token, self.config.ssl_mode).await;
        }
    }
}

// MARK: Running SQL

/// Runs a table page query: rows decoded by their prepared types, plus the raw text of the
/// values at `keys` in every row (sent back verbatim by keyset cursors).
async fn run_page(client: &Client, sql: &str, keys: &[usize]) -> Result<(QueryResult, Vec<Vec<CursorValue>>)> {
    let statement = client.prepare(sql).await.map_err(|e| query_error(&e, None))?;
    let types: Vec<Type> = statement.columns().iter().map(|c| c.type_().clone()).collect();
    let stream = client.simple_query_raw(sql).await.map_err(|e| query_error(&e, None))?;
    pin_mut!(stream);
    let (mut result, mut key_rows) = (QueryResult::default(), Vec::new());
    while let Some(message) = stream.try_next().await.map_err(|e| query_error(&e, None))? {
        if let SimpleQueryMessage::Row(row) = message {
            result.rows.push((0..row.len()).map(|i| decode(row.get(i), types.get(i))).collect());
            if !keys.is_empty() {
                let text = |i: usize| row.get(i).map_or(CursorValue::Null, |t| CursorValue::Text(t.into()));
                key_rows.push(keys.iter().map(|&i| text(i)).collect());
            }
        }
    }
    Ok((result, key_rows))
}

/// Runs a script of one or more statements and returns the last result set, or, if no
/// statement returned rows, the affected-row count of the last one.
async fn run_script(client: &Client, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
    match client.prepare(sql).await {
        Ok(statement) => {
            let types: Vec<Type> = statement.columns().iter().map(|c| c.type_().clone()).collect();
            collect(client, sql, Some(&types), max_rows, true).await
        }
        // Several statements can't be prepared; run them as-is with values left as text.
        Err(e) if is_multi_statement_error(&e) => collect(client, sql, None, max_rows, true).await,
        Err(e) => Err(query_error(&e, Some(sql))),
    }
}

fn is_multi_statement_error(e: &tokio_postgres::Error) -> bool {
    e.as_db_error().is_some_and(|db| {
        db.code() == &SqlState::SYNTAX_ERROR && db.message().contains("multiple commands")
    })
}

/// Reads every message of a simple query. Rows past `max_rows` are counted but not decoded or kept:
/// the stream is drained rather than cancelled, so later statements of a script still run.
async fn collect(
    client: &Client,
    sql: &str,
    types: Option<&[Type]>,
    max_rows: Option<u32>,
    positions: bool,
) -> Result<QueryResult> {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let located = positions.then_some(sql);
    let stream = client.simple_query_raw(sql).await.map_err(|e| query_error(&e, located))?;
    pin_mut!(stream);

    let mut current: Option<QueryResult> = None;
    let mut last_rows: Option<QueryResult> = None;
    let mut last_affected: Option<u64> = None;

    while let Some(message) = stream.try_next().await.map_err(|e| query_error(&e, located))? {
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                current = Some(QueryResult {
                    columns: columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ColumnInfo {
                            name: c.name().into(),
                            type_name: types.and_then(|t| t.get(i)).map(type_name).unwrap_or_default(),
                            is_primary_key: false,
                            is_nullable: true,
                        })
                        .collect(),
                    ..Default::default()
                });
            }
            SimpleQueryMessage::Row(row) => {
                if let Some(result) = current.as_mut() {
                    if result.rows.len() < max_rows {
                        let values = (0..row.len())
                            .map(|i| decode(row.get(i), types.and_then(|t| t.get(i))))
                            .collect();
                        result.rows.push(values);
                    } else {
                        result.truncated = true;
                        *result.total_count.get_or_insert(max_rows as u64) += 1;
                    }
                }
            }
            SimpleQueryMessage::CommandComplete(count) => match current.take() {
                Some(result) => last_rows = Some(result),
                None => last_affected = Some(count),
            },
            _ => {}
        }
    }

    Ok(last_rows.unwrap_or(QueryResult { rows_affected: last_affected, ..Default::default() }))
}

/// Turns a text-format value into a typed `Value` when the type is known.
fn decode(text: Option<&str>, ty: Option<&Type>) -> Value {
    let Some(text) = text else { return Value::Null };
    let typed = match ty {
        Some(&Type::BOOL) => match text {
            "t" => Some(Value::Bool(true)),
            "f" => Some(Value::Bool(false)),
            _ => None,
        },
        Some(&Type::INT2 | &Type::INT4 | &Type::INT8 | &Type::OID) => text.parse().ok().map(Value::Int),
        Some(&Type::FLOAT4 | &Type::FLOAT8) => text.parse().ok().map(Value::Float),
        Some(&Type::NUMERIC) => Some(Value::Decimal(text.into())),
        _ => None,
    };
    typed.unwrap_or_else(|| Value::Text(text.into()))
}

/// SQL-ish type names: `text[]` instead of `_text`.
fn type_name(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(inner) => format!("{}[]", inner.name()),
        _ => ty.name().into(),
    }
}

// MARK: Table metadata

struct TableMeta {
    relkind: String,
    reltuples: f32,
    columns: Vec<ColumnInfo>,
    primary_key: Vec<String>,
    /// Without a primary key: the smallest plain unique index on NOT NULL columns, if any.
    unique_key: Vec<String>,
}

impl TableMeta {
    async fn load(client: &Client, relation: &str, table: &TableInfo) -> Result<Self> {
        let rows = client
            .query(
                r"
                select c.relkind::text, c.reltuples, a.attname,
                       format_type(a.atttypid, a.atttypmod), a.attnotnull,
                       coalesce(a.attnum = any(i.indkey), false)
                from pg_class c
                join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
                left join pg_index i on i.indrelid = c.oid and i.indisprimary
                where c.oid = $1::text::regclass
                order by a.attnum
                ",
                &[&relation],
            )
            .await
            .map_err(|e| match e.code() {
                Some(&SqlState::UNDEFINED_TABLE) | Some(&SqlState::INVALID_SCHEMA_NAME) => {
                    Error::TableNotFound(table.qualified_name())
                }
                _ => query_error(&e, None),
            })?;

        let first = rows.first().ok_or_else(|| Error::TableNotFound(table.qualified_name()))?;
        let has_primary_key = rows.iter().any(|r| r.get::<_, bool>(5));
        let unique_key = if has_primary_key { Vec::new() } else { unique_key(client, relation).await? };
        let columns: Vec<ColumnInfo> = rows
            .iter()
            .map(|r| ColumnInfo {
                name: r.get(2),
                type_name: r.get(3),
                is_primary_key: r.get(5),
                is_nullable: !r.get::<_, bool>(4),
            })
            .collect();
        Ok(Self {
            relkind: first.get(0),
            reltuples: first.get(1),
            primary_key: columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect(),
            unique_key,
            columns,
        })
    }

    /// Page order: the user's sort, then the primary key, else a unique NOT NULL index, else the
    /// physical position (`ctid`, stable enough for paging an idle table or matview). Views,
    /// foreign tables and keyless partitioned tables have no unique tiebreak and page with OFFSET.
    fn keyset(&self, table: &TableInfo, query: &RowQuery) -> Result<Keyset> {
        let key = if self.primary_key.is_empty() { &self.unique_key } else { &self.primary_key };
        let tiebreak: Vec<SeekColumn> = if !key.is_empty() {
            key.iter().filter_map(|c| SeekColumn::column(PG, &self.columns, c)).collect()
        } else if matches!(self.relkind.as_str(), "r" | "m") {
            vec![SeekColumn::row_id("ctid", self.columns.len())]
        } else {
            Vec::new()
        };
        let enabled = !tiebreak.is_empty();
        Keyset::new(PG, table, query, &self.columns, tiebreak, enabled)
    }

    /// Exact for small tables, the planner's estimate for big unfiltered ones, else unknown.
    async fn total_count(&self, client: &Client, relation: &str, filter: Option<&str>) -> Result<Option<u64>> {
        let small = self.reltuples < EXACT_COUNT_THRESHOLD;
        Ok(match self.relkind.as_str() {
            "r" | "p" | "m" if !small && filter.is_some() => None,
            "r" | "p" | "m" if !small => Some(self.reltuples as u64),
            "r" | "p" | "m" => {
                let row = client
                    .query_one(&PG.count_query(relation, filter), &[])
                    .await
                    .map_err(|e| query_error(&e, None))?;
                Some(row.get::<_, i64>(0) as u64)
            }
            _ => None,
        })
    }
}

/// Columns of the plain (no expressions, not partial), valid unique index with the fewest key
/// columns, all NOT NULL: a unique tiebreak for paging tables without a primary key.
async fn unique_key(client: &Client, relation: &str) -> Result<Vec<String>> {
    let row = client
        .query_opt(
            r"
            select array(select a.attname from generate_series(0, i.indnkeyatts - 1) k
                         join pg_attribute a on a.attrelid = i.indrelid and a.attnum = i.indkey[k]
                         order by k)
            from pg_index i
            where i.indrelid = $1::text::regclass and i.indisunique and i.indisvalid
              and i.indpred is null and i.indexprs is null
              and not exists (select 1 from generate_series(0, i.indnkeyatts - 1) k
                              join pg_attribute a on a.attrelid = i.indrelid and a.attnum = i.indkey[k]
                              where not a.attnotnull)
            order by i.indnkeyatts, i.indexrelid
            limit 1
            ",
            &[&relation],
        )
        .await
        .map_err(|e| query_error(&e, None))?;
    Ok(row.map(|r| r.get(0)).unwrap_or_default())
}

// MARK: Structure

async fn describe(client: &Client, relation: &str, table: &TableInfo) -> Result<TableStructure> {
    let not_found = |e: tokio_postgres::Error| match e.code() {
        Some(&SqlState::UNDEFINED_TABLE) | Some(&SqlState::INVALID_SCHEMA_NAME) => Error::TableNotFound(table.qualified_name()),
        _ => query_error(&e, None),
    };
    let info = client
        .query_one(
            "select c.relkind::text, obj_description(c.oid, 'pg_class'),
                    case when c.relkind = 'p' then pg_get_partkeydef(c.oid) end,
                    case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end
             from pg_class c where c.oid = $1::text::regclass",
            &[&relation],
        )
        .await
        .map_err(not_found)?;
    let relkind: String = info.get(0);
    let table_comment: Option<String> = info.get(1);
    let partition_key: Option<String> = info.get(2);
    let view_definition: Option<String> = info.get(3);

    let primary_key: Vec<String> = client
        .query(
            "select a.attname
             from pg_index i
             cross join lateral unnest(i.indkey) with ordinality k(attnum, ord)
             join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
             where i.indrelid = $1::text::regclass and i.indisprimary
             order by k.ord",
            &[&relation],
        )
        .await
        .map_err(not_found)?
        .iter()
        .map(|r| r.get(0))
        .collect();

    let column_rows = client
        .query(
            "select a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull,
                    pg_get_expr(d.adbin, d.adrelid), col_description(a.attrelid, a.attnum),
                    a.attidentity::text, a.attgenerated::text
             from pg_attribute a
             left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
             where a.attrelid = $1::text::regclass and a.attnum > 0 and not a.attisdropped
             order by a.attnum",
            &[&relation],
        )
        .await
        .map_err(not_found)?;
    let columns: Vec<ColumnDetail> = column_rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let expression: Option<String> = r.get(3);
            let identity: String = r.get(5);
            let generated: String = r.get(6);
            let default_value = match (identity.as_str(), generated.as_str()) {
                ("a", _) => Some("GENERATED ALWAYS AS IDENTITY".to_string()),
                ("d", _) => Some("GENERATED BY DEFAULT AS IDENTITY".to_string()),
                (_, "s") => expression.map(|e| format!("GENERATED ALWAYS AS ({e}) STORED")),
                (_, "v") => expression.map(|e| format!("GENERATED ALWAYS AS ({e}) VIRTUAL")),
                _ => expression,
            };
            ColumnDetail {
                is_primary_key: primary_key.contains(&name),
                name,
                type_name: r.get(1),
                is_nullable: !r.get::<_, bool>(2),
                default_value,
                comment: r.get(4),
            }
        })
        .collect();

    let indexes: Vec<(IndexInfo, bool)> = client
        .query(
            "select ic.relname, i.indisunique, i.indisprimary, pg_get_indexdef(i.indexrelid),
                    array(select pg_get_indexdef(i.indexrelid, k, true) from generate_series(1, i.indnkeyatts) k),
                    exists(select 1 from pg_constraint c where c.conindid = i.indexrelid and c.conrelid = i.indrelid)
             from pg_index i
             join pg_class ic on ic.oid = i.indexrelid
             where i.indrelid = $1::text::regclass
             order by i.indisprimary desc, ic.relname",
            &[&relation],
        )
        .await
        .map_err(not_found)?
        .iter()
        .map(|r| {
            let index = IndexInfo {
                name: r.get(0),
                is_unique: r.get(1),
                is_primary: r.get(2),
                definition: Some(r.get(3)),
                columns: r.get(4),
            };
            (index, r.get(5))
        })
        .collect();

    let foreign_keys: Vec<ForeignKeyInfo> = client
        .query(
            "select con.conname,
                    array(select a.attname from unnest(con.conkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.o),
                    fn.nspname, fc.relname,
                    array(select a.attname from unnest(con.confkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.o),
                    con.confupdtype::text, con.confdeltype::text
             from pg_constraint con
             join pg_class fc on fc.oid = con.confrelid
             join pg_namespace fn on fn.oid = fc.relnamespace
             where con.conrelid = $1::text::regclass and con.contype = 'f'
             order by con.conname",
            &[&relation],
        )
        .await
        .map_err(not_found)?
        .iter()
        .map(|r| ForeignKeyInfo {
            name: r.get(0),
            columns: r.get(1),
            referenced_schema: r.get(2),
            referenced_table: r.get(3),
            referenced_columns: r.get(4),
            on_update: fk_action(&r.get::<_, String>(5)).into(),
            on_delete: fk_action(&r.get::<_, String>(6)).into(),
        })
        .collect();

    // Clones of a key on partitions (or pointing at this table's partitions) have a parent: skip them.
    let referenced_by: Vec<ReferencingKey> = client
        .query(
            "select n.nspname, c.relname, con.conname,
                    array(select a.attname from unnest(con.conkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.o),
                    array(select a.attname from unnest(con.confkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.o)
             from pg_constraint con
             join pg_class c on c.oid = con.conrelid
             join pg_namespace n on n.oid = c.relnamespace
             where con.confrelid = $1::text::regclass and con.contype = 'f' and con.conparentid = 0
             order by n.nspname, c.relname, con.conname",
            &[&relation],
        )
        .await
        .map_err(not_found)?
        .iter()
        .map(|r| ReferencingKey { schema: r.get(0), table: r.get(1), name: r.get(2), columns: r.get(3), referenced_columns: r.get(4) })
        .collect();

    let constraints: Vec<(String, String)> = client
        .query(
            "select conname, pg_get_constraintdef(oid, true)
             from pg_constraint
             where conrelid = $1::text::regclass and contype in ('p', 'u', 'f', 'c', 'x')
             order by case contype when 'p' then 0 when 'u' then 1 when 'f' then 2 else 3 end, conname",
            &[&relation],
        )
        .await
        .map_err(not_found)?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();

    // MARK: DDL
    let mut ddl = String::new();
    match (relkind.as_str(), view_definition) {
        ("v" | "m", Some(definition)) => {
            let kind = if relkind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
            let body = definition.trim().trim_end_matches(';');
            ddl.push_str(&format!("CREATE {kind} {relation} AS\n{body};\n"));
        }
        _ => {
            let mut lines: Vec<String> = column_rows
                .iter()
                .zip(&columns)
                .map(|(r, c)| {
                    let mut line = format!("    {} {}", quote_ident(&c.name), c.type_name);
                    let plain_default = r.get::<_, String>(5).is_empty() && r.get::<_, String>(6).is_empty();
                    match &c.default_value {
                        Some(d) if plain_default => line.push_str(&format!(" DEFAULT {d}")),
                        Some(d) => line.push_str(&format!(" {d}")),
                        None => {}
                    }
                    if !c.is_nullable {
                        line.push_str(" NOT NULL");
                    }
                    line
                })
                .collect();
            lines.extend(constraints.iter().map(|(name, def)| format!("    CONSTRAINT {} {def}", quote_ident(name))));
            let foreign = if relkind == "f" { "FOREIGN " } else { "" };
            ddl.push_str(&format!("CREATE {foreign}TABLE {relation} (\n{}\n)", lines.join(",\n")));
            if let Some(key) = partition_key {
                ddl.push_str(&format!(" PARTITION BY {key}"));
            }
            ddl.push_str(";\n");
        }
    }
    for (index, backs_constraint) in &indexes {
        if let (false, Some(definition)) = (backs_constraint, &index.definition) {
            ddl.push_str(&format!("\n{definition};"));
        }
    }
    if indexes.iter().any(|(_, backs)| !backs) {
        ddl.push('\n');
    }
    if let Some(comment) = table_comment {
        let kind = match relkind.as_str() {
            "v" => "VIEW",
            "m" => "MATERIALIZED VIEW",
            _ => "TABLE",
        };
        ddl.push_str(&format!("\nCOMMENT ON {kind} {relation} IS {};", PG.quote_literal(&comment)));
    }
    for c in &columns {
        if let Some(comment) = &c.comment {
            ddl.push_str(&format!("\nCOMMENT ON COLUMN {relation}.{} IS {};", quote_ident(&c.name), PG.quote_literal(comment)));
        }
    }

    Ok(TableStructure {
        columns,
        primary_key,
        indexes: indexes.into_iter().map(|(i, _)| i).collect(),
        foreign_keys,
        referenced_by,
        ddl: Some(ddl.trim_end().to_string()),
    })
}

/// `pg_constraint.confupdtype` / `confdeltype` codes.
fn fk_action(code: &str) -> &'static str {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

const PG: Dialect = Dialect(DatabaseKind::Postgres);

fn quote_ident(name: &str) -> String {
    PG.quote_ident(name)
}

fn quote_relation(schema: &str, name: &str) -> String {
    PG.quote_relation(schema, name)
}

// MARK: Cancellation

async fn send_cancel(token: CancelToken, mode: SslMode) -> Result<()> {
    let tls: MakeRustlsConnect = tls::connector(mode)?;
    token.cancel_query(tls).await.map_err(|e| Error::ConnectionFailed(error_chain(&e)))
}

/// Sends a cancel request for the in-flight query unless disarmed before being dropped.
struct CancelOnDrop {
    token: Option<CancelToken>,
    mode: SslMode,
}

impl CancelOnDrop {
    fn new(token: CancelToken, mode: SslMode) -> Self {
        Self { token: Some(token), mode }
    }

    fn disarm(mut self) {
        self.token = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else { return };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mode = self.mode;
            handle.spawn(async move {
                let _ = send_cancel(token, mode).await;
            });
        }
    }
}

// MARK: Errors

/// `ERROR: message` plus DETAIL/HINT and, when we have the SQL, the line and column.
pub(crate) fn query_error(e: &tokio_postgres::Error, sql: Option<&str>) -> Error {
    let Some(db) = e.as_db_error() else {
        return if e.is_closed() {
            Error::ConnectionFailed("the server closed the connection".into())
        } else {
            Error::Query(error_chain(e))
        };
    };
    if db.code() == &SqlState::QUERY_CANCELED {
        return Error::Cancelled;
    }

    let mut message = format!("{}: {}", db.severity(), db.message());
    if let (Some(sql), Some(tokio_postgres::error::ErrorPosition::Original(position))) = (sql, db.position()) {
        let (line, column) = line_column(sql, (*position as usize).saturating_sub(1));
        message.push_str(&format!(" (line {line}, column {column})"));
    }
    if let Some(detail) = db.detail() {
        message.push_str(&format!("\nDETAIL: {detail}"));
    }
    if let Some(hint) = db.hint() {
        message.push_str(&format!("\nHINT: {hint}"));
    }
    Error::Query(message)
}

/// "error connecting to server: Connection refused (os error 61)" instead of just the top level.
/// Server-reported startup errors (bad database, password…) without the "db error: FATAL:" noise.
fn connect_error(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => match db.hint() {
            Some(hint) => format!("{} ({hint})", db.message()),
            None => db.message().to_string(),
        },
        None => error_chain(e),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_values_by_type() {
        assert_eq!(decode(None, Some(&Type::INT8)), Value::Null);
        assert_eq!(decode(Some("42"), Some(&Type::INT4)), Value::Int(42));
        assert_eq!(decode(Some("t"), Some(&Type::BOOL)), Value::Bool(true));
        assert_eq!(decode(Some("1.5"), Some(&Type::FLOAT8)), Value::Float(1.5));
        assert_eq!(decode(Some("NaN"), Some(&Type::NUMERIC)), Value::Decimal("NaN".into()));
        assert_eq!(decode(Some("{a,b}"), Some(&Type::TEXT_ARRAY)), Value::Text("{a,b}".into()));
        assert_eq!(decode(Some("7"), None), Value::Text("7".into()));
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_relation("public", r#"we"ird"#), r#""public"."we""ird""#);
    }

}
