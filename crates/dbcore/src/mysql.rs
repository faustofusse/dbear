//! MySQL / MariaDB driver (mysql_async + rustls).
//!
//! In MySQL a database *is* a schema. Like Postgres, a connection browses one database (its
//! `database`, also the sessions' default, so scripts can use unqualified names); the others are
//! listed by `list_databases` and opened as their own connection (`ConnectionConfig::with_database`).
//! Without a configured database, every database is listed as a schema section instead.
//! Tables are always addressed as `` `db`.`table` ``.
//!
//! Values come over the text protocol (`COM_QUERY`), exactly as the mysql client prints them;
//! column metadata turns them into typed values (ints, floats, exact decimals, booleans).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Column, Conn, Opts, OptsBuilder, SslOpts};
use tokio::sync::{Mutex, MutexGuard};

use crate::dialect::{error_chain, hex_preview, Dialect};
use crate::access::{self, DatabaseAccess, DatabaseLevelContext, Grant, PrivilegeSet, Role, RoleRef};
use crate::driver::{Driver, Error, Result};
use crate::edit::{self, EditStatement};
use crate::keyset::{page_sql, CursorValue, Keyset, PageCursor, RowPage, SeekColumn, Start};
use crate::model::*;

const MYSQL: Dialect = Dialect(DatabaseKind::Mysql);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle sessions are pinged before reuse: the server drops them after `wait_timeout`.
const PING_AFTER_IDLE: Duration = Duration::from_secs(60);
/// Tables smaller than this (by the server's estimate) get an exact `count(*)`.
const EXACT_COUNT_THRESHOLD: u64 = 100_000;
const SYSTEM_SCHEMAS: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];
/// Character set number of binary strings (BLOB, VARBINARY…).
const BINARY_CHARSET: u16 = 63;
const ER_QUERY_INTERRUPTED: u16 = 1317;

pub struct MysqlDriver {
    config: ConnectionConfig,
    /// Catalog and table browsing.
    browse: Session,
    /// User scripts: a long query doesn't block browsing, and `cancel` only hits scripts.
    query: Session,
    /// Saving row edits, in a transaction of its own. Reports *matched* rows (`CLIENT_FOUND_ROWS`):
    /// an UPDATE that writes the value a cell already has still matched its row.
    edit: Session,
    /// Set by `cancel`, so an interrupted script reports "cancelled" even when the server
    /// just ends it early (e.g. `sleep()` returns 1 instead of failing).
    cancelled: AtomicBool,
}

#[derive(Default)]
struct Session {
    conn: Mutex<Option<(Conn, Instant)>>,
    /// Server thread id of the open connection (0 = none), readable while a query holds `conn`.
    thread_id: AtomicU32,
    /// Affected-row counts are rows matched, not rows changed.
    found_rows: bool,
}

/// A locked, open session connection.
struct Lease<'a> {
    guard: MutexGuard<'a, Option<(Conn, Instant)>>,
    session: &'a Session,
}

impl Lease<'_> {
    fn conn(&mut self) -> &mut Conn {
        &mut self.guard.as_mut().expect("leased sessions are open").0
    }

    /// Forgets a connection that hit a network error, so the next call reconnects.
    fn check<T>(&mut self, result: std::result::Result<T, mysql_async::Error>) -> std::result::Result<T, mysql_async::Error> {
        if let Err(mysql_async::Error::Io(_) | mysql_async::Error::Driver(_)) = &result {
            self.guard.take();
            self.session.thread_id.store(0, Ordering::Relaxed);
        }
        result
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some((_, used)) = self.guard.as_mut() {
            *used = Instant::now();
        }
    }
}

impl Session {
    async fn lease(&self, config: &ConnectionConfig) -> Result<Lease<'_>> {
        let mut guard = self.conn.lock().await;
        if let Some((conn, used)) = guard.as_mut() {
            if used.elapsed() > PING_AFTER_IDLE && conn.ping().await.is_err() {
                guard.take();
            }
        }
        if guard.is_none() {
            let conn = connect_with(config, self.found_rows).await?;
            self.thread_id.store(conn.id(), Ordering::Relaxed);
            *guard = Some((conn, Instant::now()));
        }
        Ok(Lease { guard, session: self })
    }

    fn is_open(&self) -> bool {
        self.thread_id.load(Ordering::Relaxed) != 0
    }

    async fn close(&self) {
        self.thread_id.store(0, Ordering::Relaxed);
        if let Some((conn, _)) = self.conn.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.disconnect()).await;
        }
    }
}

impl MysqlDriver {
    pub fn new(config: ConnectionConfig) -> Self {
        Self {
            config,
            browse: Session::default(),
            query: Session::default(),
            edit: Session { found_rows: true, ..Session::default() },
            cancelled: AtomicBool::new(false),
        }
    }

    /// The database this connection browses; `None` = no database configured, list them all.
    fn only_database(&self) -> Option<&str> {
        let database = self.config.database.trim();
        (!database.is_empty()).then_some(database)
    }

    /// `schema_name not in (…system schemas…)`, for `column`.
    fn user_databases(column: &str) -> String {
        let system = SYSTEM_SCHEMAS.iter().map(|s| MYSQL.quote_literal(s)).collect::<Vec<_>>().join(", ");
        format!("{column} not in ({system})")
    }
}

fn opts(config: &ConnectionConfig, tls: Option<SslOpts>, found_rows: bool) -> Opts {
    let database = config.database.trim();
    OptsBuilder::default()
        .ip_or_hostname(config.host.trim())
        .tcp_port(config.port.unwrap_or(3306))
        .user(Some(config.user.as_deref().unwrap_or("root")))
        .pass(config.password.clone())
        .db_name((!database.is_empty()).then(|| database.to_string()))
        // `localhost` would otherwise switch to the server's unix socket, which may not be ours (containers).
        .prefer_socket(false)
        .ssl_opts(tls)
        .client_found_rows(found_rows)
        .into()
}

pub(crate) async fn connect(config: &ConnectionConfig) -> Result<Conn> {
    connect_with(config, false).await
}

async fn connect_with(config: &ConnectionConfig, found_rows: bool) -> Result<Conn> {
    let attempt = |tls: Option<SslOpts>| async move {
        match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts(config, tls, found_rows))).await {
            Ok(result) => result,
            Err(_) => Err(mysql_async::Error::Other("timed out".into())),
        }
    };
    let insecure = || SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
    let result = match config.ssl_mode {
        SslMode::Disable => attempt(None).await,
        SslMode::Require => attempt(Some(insecure())).await,
        SslMode::VerifyFull => attempt(Some(SslOpts::default())).await,
        // Like libpq's "prefer": TLS when the server offers it, plain otherwise.
        SslMode::Prefer => match attempt(Some(insecure())).await {
            Err(e) if !matches!(e, mysql_async::Error::Server(_)) => attempt(None).await.map_err(|_| e),
            other => other,
        },
    };
    result.map_err(|e| Error::ConnectionFailed(connect_error(&e)))
}

#[async_trait]
impl Driver for MysqlDriver {
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

    /// Every user database on the server (system ones hidden), whichever one this connection browses.
    async fn list_databases(&self) -> Result<Vec<String>> {
        let sql = format!(
            "select schema_name from information_schema.schemata where {} order by schema_name",
            Self::user_databases("schema_name")
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let result = lease.conn().query::<String, _>(sql).await;
        lease.check(result).map_err(|e| query_error(&e))
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        let filter = match self.only_database() {
            Some(db) => format!("s.schema_name = {}", MYSQL.quote_literal(db)),
            None => Self::user_databases("s.schema_name"),
        };
        // Empty databases are listed too (left join), so they show up as empty sections.
        let sql = format!(
            "select s.schema_name, t.table_name, t.table_type, t.table_rows
             from information_schema.schemata s
             left join information_schema.tables t on t.table_schema = s.schema_name
             where {filter}
             order by s.schema_name, t.table_name"
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let result = lease.conn().query::<(String, Option<String>, Option<String>, Option<u64>), _>(sql).await;
        let rows = lease.check(result).map_err(|e| query_error(&e))?;

        let mut schemas: Vec<Schema> = Vec::new();
        for (schema, table, table_type, table_rows) in rows {
            if schemas.last().is_none_or(|s| s.name != schema) {
                schemas.push(Schema { name: schema.clone(), tables: Vec::new() });
            }
            let Some(name) = table else { continue };
            let kind = if table_type.as_deref() == Some("VIEW") { TableKind::View } else { TableKind::Table };
            schemas.last_mut().unwrap().tables.push(TableInfo {
                schema,
                name,
                kind,
                // InnoDB estimates are cached (`information_schema_stats_expiry`, 24h by default), so a
                // fresh or never-analyzed table reports 0: show no count rather than a wrong one.
                estimated_row_count: if kind == TableKind::View { None } else { table_rows.filter(|&n| n > 0) },
            });
        }
        Ok(schemas)
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        let filter = match self.only_database() {
            Some(db) => format!("table_schema = {}", MYSQL.quote_literal(db)),
            None => Self::user_databases("table_schema"),
        };
        let sql = format!(
            "select table_schema, table_name, column_name, column_type, is_nullable, column_key
             from information_schema.columns
             where {filter}
             order by table_schema, table_name, ordinal_position"
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let result = lease.conn().query::<(String, String, String, String, String, String), _>(sql).await;
        let rows = lease.check(result).map_err(|e| query_error(&e))?;

        let mut tables: Vec<TableColumns> = Vec::new();
        for (schema, table, column, type_name, nullable, key) in rows {
            if tables.last().is_none_or(|t| t.schema != schema || t.table != table) {
                tables.push(TableColumns { schema, table, columns: Vec::new() });
            }
            tables.last_mut().unwrap().columns.push(ColumnInfo {
                name: column,
                type_name,
                is_primary_key: key == "PRI",
                is_nullable: nullable == "YES",
            });
        }
        Ok(tables)
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        let mut lease = self.browse.lease(&self.config).await?;
        let meta = TableMeta::load(&mut lease, table).await?;
        let relation = MYSQL.quote_relation(&table.schema, &table.name);
        let (keyset, _) = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();
        let sql = page_sql(&relation, &[], filter, None, &keyset.order_by(), u64::from(limit), offset);

        let result = run_page(lease.conn(), &sql, &[]).await;
        let (mut result, _) = lease.check(result).map_err(|e| query_error(&e))?;
        result.columns = meta.columns.clone();
        if offset == 0 {
            result.total_count = meta.total_count(&mut lease, &relation, filter).await?;
        }
        Ok(result)
    }

    async fn fetch_page(&self, table: &TableInfo, query: &RowQuery, limit: u32, after: Option<&PageCursor>) -> Result<RowPage> {
        let mut lease = self.browse.lease(&self.config).await?;
        let meta = TableMeta::load(&mut lease, table).await?;
        let relation = MYSQL.quote_relation(&table.schema, &table.name);
        let (keyset, kinds) = meta.keyset(table, query)?;
        let filter = query.filter.as_deref();

        let start = keyset.start(after, |i, v| render_key(kinds[i], v));
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
            let sql = page_sql(&relation, &[], filter, seek.as_deref(), &keyset.order_by(), need, offset);
            let part = run_page(lease.conn(), &sql, &keyset.key_indexes()).await;
            let (part, part_keys) = lease.check(part).map_err(|e| query_error(&e))?;
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
        // Fails with "table not found" before the catalog queries below quietly return nothing.
        TableMeta::load(&mut lease, table).await?;
        describe(&mut lease, table).await
    }

    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        let mut lease = self.query.lease(&self.config).await?;
        self.cancelled.store(false, Ordering::SeqCst);
        // If this future is dropped mid-query, stop it on the server too.
        let guard = KillOnDrop { config: Some(self.config.clone()), thread_id: lease.conn().id() };
        let result = run_script(lease.conn(), sql, max_rows).await;
        let result = lease.check(result);
        guard.disarm();
        match result {
            _ if self.cancelled.swap(false, Ordering::SeqCst) => Err(Error::Cancelled),
            Ok(result) => Ok(result),
            Err(e) => Err(query_error(&e)),
        }
    }

    async fn apply(&self, statements: &[EditStatement]) -> Result<u64> {
        let mut lease = self.edit.lease(&self.config).await?;
        // `rollback` first: a save abandoned midway (its future dropped) must never be committed later.
        for sql in ["rollback", "start transaction"] {
            let result = lease.conn().query_drop(sql).await;
            lease.check(result).map_err(|e| query_error(&e))?;
        }
        let mut total = 0;
        for statement in statements {
            let result = lease.conn().query_drop(statement.sql.as_str()).await;
            let result = match lease.check(result) {
                Ok(()) => {
                    let affected = lease.conn().affected_rows();
                    edit::check_affected(statement, affected).map(|()| affected)
                }
                Err(e) => Err(edit::failed(statement, query_error(&e))),
            };
            match result {
                Ok(affected) => total += affected,
                Err(e) => {
                    let _ = lease.conn().query_drop("rollback").await;
                    return Err(e);
                }
            }
        }
        let result = lease.conn().query_drop("commit").await;
        lease.check(result).map_err(|e| query_error(&e))?;
        Ok(total)
    }

    async fn cancel(&self) {
        let thread_id = self.query.thread_id.load(Ordering::Relaxed);
        if thread_id == 0 {
            return;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = kill_query(&self.config, thread_id).await;
    }

    async fn list_roles(&self) -> Result<Vec<Role>> {
        let mut lease = self.browse.lease(&self.config).await?;
        // MariaDB's `mysql.user` has no `account_locked` before 10.4: fall back to "not locked".
        let queries = [
            "select User, Host, account_locked, Super_priv, max_user_connections from mysql.user order by User, Host",
            "select User, Host, 'N', Super_priv, max_user_connections from mysql.user order by User, Host",
        ];
        let mut accounts = Vec::new();
        for (i, sql) in queries.iter().enumerate() {
            let result = lease.conn().query::<(String, String, String, String, u64), _>(*sql).await;
            match lease.check(result) {
                Ok(rows) => {
                    accounts = rows;
                    break;
                }
                Err(e) if i == queries.len() - 1 => return Err(query_error(&e)),
                Err(_) => {}
            }
        }
        // Role grants (MySQL 8); missing elsewhere, then nobody is a member of anything.
        let result = lease
            .conn()
            .query::<(String, String, String, String), _>("select FROM_USER, FROM_HOST, TO_USER, TO_HOST from mysql.role_edges")
            .await;
        let edges = lease.check(result).unwrap_or_default();
        Ok(accounts
            .into_iter()
            .map(|(name, host, locked, super_priv, max_connections)| {
                let member_of = edges
                    .iter()
                    .filter(|(_, _, to_user, to_host)| *to_user == name && *to_host == host)
                    .map(|(user, host, _, _)| RoleRef::new(user, Some(host.clone())))
                    .collect();
                Role {
                    is_system: name.starts_with("mysql."),
                    can_login: !locked.eq_ignore_ascii_case("Y"),
                    is_superuser: super_priv.eq_ignore_ascii_case("Y"),
                    can_create_db: false,
                    can_create_role: false,
                    connection_limit: u32::try_from(max_connections).ok().filter(|&n| n > 0),
                    valid_until: None,
                    member_of,
                    comment: None,
                    name,
                    host: Some(host),
                }
            })
            .collect())
    }

    async fn list_grants(&self, role: &RoleRef) -> Result<Vec<Grant>> {
        // information_schema spells grantees `'user'@'host'`.
        let account = format!("'{}'@'{}'", role.name.replace('\'', "''"), role.host.as_deref().unwrap_or("%").replace('\'', "''"));
        let grantee = MYSQL.quote_literal(&account);
        // `USAGE` means "no privileges" and isn't a grant to show.
        let sql = format!(
            "select 'server', null, null, privilege_type, is_grantable from information_schema.user_privileges
               where grantee = {grantee} and privilege_type <> 'USAGE'
             union all
             select 'database', null, table_schema, privilege_type, is_grantable from information_schema.schema_privileges
               where grantee = {grantee}
             union all
             select 'table', table_schema, table_name, privilege_type, is_grantable from information_schema.table_privileges
               where grantee = {grantee}"
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let result = lease.conn().query::<(String, Option<String>, Option<String>, String, String), _>(sql).await;
        let rows = lease.check(result).map_err(|e| query_error(&e))?;
        let mut grants: Vec<Grant> = rows
            .into_iter()
            .filter_map(|(kind, schema, name, privilege, grantable)| {
                let object = access::object_from_catalog(&kind, schema, name)?;
                Some(Grant { object, privilege, grantable: grantable.eq_ignore_ascii_case("YES") })
            })
            .collect();
        grants.sort();
        Ok(grants)
    }

    async fn list_database_access(&self, role: &RoleRef) -> Result<Vec<DatabaseAccess>> {
        let account = format!("'{}'@'{}'", role.name.replace('\'', "''"), role.host.as_deref().unwrap_or("%").replace('\'', "''"));
        let grantee = MYSQL.quote_literal(&account);
        let sql = format!(
            "select s.schema_name,
                    group_concat(p.privilege_type order by p.privilege_type separator ','),
                    coalesce(min(p.is_grantable = 'YES'), 0)
             from information_schema.schemata s
             left join information_schema.schema_privileges p on p.table_schema = s.schema_name and p.grantee = {grantee}
             where {}
             group by s.schema_name
             order by s.schema_name",
            Self::user_databases("s.schema_name")
        );
        let mut lease = self.browse.lease(&self.config).await?;
        let result = lease.conn().query::<(String, Option<String>, i64), _>(sql).await;
        let rows = lease.check(result).map_err(|e| query_error(&e))?;
        Ok(rows
            .into_iter()
            .map(|(database, privileges, grantable)| {
                let privileges: Vec<String> = privileges.map(|p| p.split(',').map(str::to_string).collect()).unwrap_or_default();
                DatabaseAccess {
                    database,
                    level: access::mysql_level(&privileges),
                    privileges: PrivilegeSet { grantable: grantable != 0 && !privileges.is_empty(), privileges },
                    everyone_can_connect: false,
                    is_owner: false,
                }
            })
            .collect())
    }

    async fn database_level(&self, role: &RoleRef, database: &str) -> Result<DatabaseLevelContext> {
        let access = self.list_database_access(role).await?;
        let found = access
            .into_iter()
            .find(|a| a.database == database)
            .ok_or_else(|| Error::Query(format!("No database “{database}”")))?;
        Ok(DatabaseLevelContext { database: found.database, level: found.level, privileges: found.privileges, schemas: vec![], owners: vec![] })
    }
}

/// `KILL QUERY` must come from another connection: the script's own one is busy.
async fn kill_query(config: &ConnectionConfig, thread_id: u32) -> Result<()> {
    let mut conn = connect(config).await?;
    let result = conn.query_drop(format!("kill query {thread_id}")).await;
    let _ = conn.disconnect().await;
    result.map_err(|e| query_error(&e))
}

struct KillOnDrop {
    config: Option<ConnectionConfig>,
    thread_id: u32,
}

impl KillOnDrop {
    fn disarm(mut self) {
        self.config = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let Some(config) = self.config.take() else { return };
        let thread_id = self.thread_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = kill_query(&config, thread_id).await;
            });
        }
    }
}

// MARK: Table metadata

struct TableMeta {
    columns: Vec<ColumnInfo>,
    primary_key: Vec<String>,
    /// Without a primary key: the smallest plain unique index on NOT NULL columns, if any.
    unique_key: Vec<String>,
    /// Server estimate for base tables; `None` for views.
    estimated_rows: Option<u64>,
}

impl TableMeta {
    async fn load(lease: &mut Lease<'_>, table: &TableInfo) -> Result<Self> {
        let schema = MYSQL.quote_literal(&table.schema);
        let name = MYSQL.quote_literal(&table.name);
        let info = lease
            .conn()
            .query_first::<(String, Option<u64>), _>(format!(
                "select table_type, table_rows from information_schema.tables where table_schema = {schema} and table_name = {name}"
            ))
            .await;
        let (table_type, table_rows) =
            lease.check(info).map_err(|e| query_error(&e))?.ok_or_else(|| Error::TableNotFound(table.qualified_name()))?;

        let columns = lease
            .conn()
            .query::<(String, String, String, String), _>(format!(
                "select column_name, column_type, is_nullable, column_key from information_schema.columns
                 where table_schema = {schema} and table_name = {name} order by ordinal_position"
            ))
            .await;
        let columns: Vec<ColumnInfo> = lease
            .check(columns)
            .map_err(|e| query_error(&e))?
            .into_iter()
            .map(|(name, type_name, nullable, key)| ColumnInfo {
                name,
                type_name,
                is_primary_key: key == "PRI",
                is_nullable: nullable == "YES",
            })
            .collect();
        let primary_key: Vec<String> = columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
        let unique_key = if primary_key.is_empty() && table_type != "VIEW" {
            unique_key(lease, &schema, &name, &columns).await?
        } else {
            Vec::new()
        };
        Ok(Self {
            primary_key,
            unique_key,
            columns,
            estimated_rows: if table_type == "VIEW" { None } else { Some(table_rows.unwrap_or(0)) },
        })
    }

    /// Page order: the user's sort, then the primary key (else a unique NOT NULL index). Seeking
    /// needs such a key, and sort types that compare the way they sort (see [`key_kind`]); the
    /// rest pages with OFFSET. Also returns how to spell each term's values.
    fn keyset(&self, table: &TableInfo, query: &RowQuery) -> Result<(Keyset, Vec<KeyKind>)> {
        let key = if self.primary_key.is_empty() { &self.unique_key } else { &self.primary_key };
        let tiebreak: Vec<SeekColumn> = key.iter().filter_map(|c| SeekColumn::column(MYSQL, &self.columns, c)).collect();
        let mut keyset = Keyset::new(MYSQL, table, query, &self.columns, tiebreak, !key.is_empty())?;
        let kinds: Option<Vec<KeyKind>> = keyset.columns.iter().map(|c| key_kind(&self.columns[c.index].type_name)).collect();
        keyset.enabled &= kinds.is_some();
        Ok((keyset, kinds.unwrap_or_default()))
    }

    /// Exact for small tables, the server's estimate for big unfiltered ones, else unknown.
    async fn total_count(&self, lease: &mut Lease<'_>, relation: &str, filter: Option<&str>) -> Result<Option<u64>> {
        Ok(match self.estimated_rows {
            None => None,
            Some(rows) if rows >= EXACT_COUNT_THRESHOLD && filter.is_some() => None,
            Some(rows) if rows >= EXACT_COUNT_THRESHOLD => Some(rows),
            Some(_) => {
                let count = lease.conn().query_first::<u64, _>(MYSQL.count_query(relation, filter)).await;
                lease.check(count).map_err(|e| query_error(&e))?
            }
        })
    }
}

/// Columns of the unique index with the fewest columns whose parts are all whole (no prefix,
/// no expression) NOT NULL columns: a unique tiebreak for paging tables without a primary key.
/// `schema` and `name` are quoted literals.
async fn unique_key(lease: &mut Lease<'_>, schema: &str, name: &str, columns: &[ColumnInfo]) -> Result<Vec<String>> {
    let parts = lease
        .conn()
        .query::<(String, Option<String>, Option<u64>), _>(format!(
            "select index_name, column_name, sub_part from information_schema.statistics
             where table_schema = {schema} and table_name = {name} and non_unique = 0 and index_name <> 'PRIMARY'
             order by index_name, seq_in_index"
        ))
        .await;
    let parts = lease.check(parts).map_err(|e| query_error(&e))?;
    let mut indexes: Vec<(String, Option<Vec<String>>)> = Vec::new();
    for (index, column, sub_part) in parts {
        if indexes.last().is_none_or(|(n, _)| *n != index) {
            indexes.push((index, Some(Vec::new())));
        }
        let usable = column.filter(|c| sub_part.is_none() && columns.iter().any(|k| &k.name == c && !k.is_nullable));
        let entry = &mut indexes.last_mut().unwrap().1;
        match (usable, entry.as_mut()) {
            (Some(c), Some(list)) => list.push(c),
            _ => *entry = None,
        }
    }
    Ok(indexes.into_iter().filter_map(|(_, c)| c).min_by_key(Vec::len).unwrap_or_default())
}

/// How a sort key's value is written back into a seek predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// Bare: comparing a BIGINT with a string would go through doubles and lose precision.
    Number,
    /// A string literal, compared in the column's collation (or converted to a date/time).
    Quoted,
    /// `X'…'` for binary strings.
    Hex,
}

/// Types whose `>`/`<` agree with `ORDER BY`. Not FLOAT (its f32 values never equal their decimal
/// text), ENUM/SET (sorted by position, compared as strings), BIT, JSON, spatial types, and
/// TEXT/BLOB (sorted by their first `max_sort_length` bytes only).
fn key_kind(column_type: &str) -> Option<KeyKind> {
    let base = column_type.split(['(', ' ']).next().unwrap_or_default().to_ascii_lowercase();
    match base.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "double"
        | "real" | "year" => Some(KeyKind::Number),
        "char" | "varchar" | "date" | "datetime" | "timestamp" | "time" => Some(KeyKind::Quoted),
        "binary" | "varbinary" => Some(KeyKind::Hex),
        _ => None,
    }
}

fn render_key(kind: KeyKind, value: &CursorValue) -> String {
    let bytes: &[u8] = match value {
        CursorValue::Text(t) => t.as_bytes(),
        CursorValue::Bytes(b) => b,
        CursorValue::Int(i) => return i.to_string(),
        CursorValue::Float(bits) => return f64::from_bits(*bits).to_string(),
        CursorValue::Null => unreachable!("NULL keys are never rendered"),
    };
    let numeric = !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_digit() || b"+-.eE".contains(b));
    match kind {
        KeyKind::Number if numeric => String::from_utf8_lossy(bytes).into_owned(),
        KeyKind::Hex => format!("X'{}'", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        _ => MYSQL.quote_literal(&String::from_utf8_lossy(bytes)),
    }
}

// MARK: Structure

async fn describe(lease: &mut Lease<'_>, table: &TableInfo) -> Result<TableStructure> {
    let schema = MYSQL.quote_literal(&table.schema);
    let name = MYSQL.quote_literal(&table.name);
    let relation = MYSQL.quote_relation(&table.schema, &table.name);

    let primary_key = lease
        .conn()
        .query::<String, _>(format!(
            "select column_name from information_schema.key_column_usage
             where table_schema = {schema} and table_name = {name} and constraint_name = 'PRIMARY'
             order by ordinal_position"
        ))
        .await;
    let primary_key = lease.check(primary_key).map_err(|e| query_error(&e))?;

    type ColumnRow = (String, String, String, Option<String>, String, String, Option<String>);
    let columns = lease
        .conn()
        .query::<ColumnRow, _>(format!(
            "select column_name, column_type, is_nullable, column_default, column_comment, extra, generation_expression
             from information_schema.columns
             where table_schema = {schema} and table_name = {name}
             order by ordinal_position"
        ))
        .await;
    let columns = lease
        .check(columns)
        .map_err(|e| query_error(&e))?
        .into_iter()
        .map(|(name, type_name, nullable, default, comment, extra, generated)| ColumnDetail {
            is_primary_key: primary_key.contains(&name),
            default_value: column_default(default, &extra, generated),
            name,
            type_name,
            is_nullable: nullable == "YES",
            comment: (!comment.is_empty()).then_some(comment),
        })
        .collect();

    let index_rows = lease
        .conn()
        .query::<(String, i64, Option<String>), _>(format!(
            "select index_name, non_unique, column_name
             from information_schema.statistics
             where table_schema = {schema} and table_name = {name}
             order by index_name = 'PRIMARY' desc, index_name, seq_in_index"
        ))
        .await;
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for (index, non_unique, column) in lease.check(index_rows).map_err(|e| query_error(&e))? {
        if indexes.last().is_none_or(|i| i.name != index) {
            indexes.push(IndexInfo {
                is_primary: index == "PRIMARY",
                name: index,
                columns: Vec::new(),
                is_unique: non_unique == 0,
                definition: None,
            });
        }
        // Functional indexes (MySQL 8.0.13+) have no column name.
        indexes.last_mut().expect("pushed above").columns.push(column.unwrap_or_else(|| "<expression>".into()));
    }

    let fk_rows = lease
        .conn()
        .query::<(String, String, String, String, String, String, String), _>(format!(
            "select k.constraint_name, k.column_name, k.referenced_table_schema, k.referenced_table_name,
                    k.referenced_column_name, r.update_rule, r.delete_rule
             from information_schema.key_column_usage k
             join information_schema.referential_constraints r
               on r.constraint_schema = k.constraint_schema and r.constraint_name = k.constraint_name
              and r.table_name = k.table_name
             where k.table_schema = {schema} and k.table_name = {name} and k.referenced_table_name is not null
             order by k.constraint_name, k.ordinal_position"
        ))
        .await;
    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for (constraint, column, ref_schema, ref_table, ref_column, on_update, on_delete) in
        lease.check(fk_rows).map_err(|e| query_error(&e))?
    {
        if foreign_keys.last().is_none_or(|f| f.name != constraint) {
            foreign_keys.push(ForeignKeyInfo {
                name: constraint,
                columns: Vec::new(),
                referenced_schema: ref_schema,
                referenced_table: ref_table,
                referenced_columns: Vec::new(),
                on_update,
                on_delete,
            });
        }
        let fk = foreign_keys.last_mut().expect("pushed above");
        fk.columns.push(column);
        fk.referenced_columns.push(ref_column);
    }

    let incoming_rows = lease
        .conn()
        .query::<(String, String, String, String, String), _>(format!(
            "select table_schema, table_name, constraint_name, column_name, referenced_column_name
             from information_schema.key_column_usage
             where referenced_table_schema = {schema} and referenced_table_name = {name}
             order by table_schema, table_name, constraint_name, ordinal_position"
        ))
        .await;
    let mut referenced_by: Vec<ReferencingKey> = Vec::new();
    for (ref_schema, ref_table, constraint, column, referenced_column) in lease.check(incoming_rows).map_err(|e| query_error(&e))? {
        if referenced_by.last().is_none_or(|k| (&k.schema, &k.table, &k.name) != (&ref_schema, &ref_table, &constraint)) {
            referenced_by.push(ReferencingKey {
                schema: ref_schema,
                table: ref_table,
                name: constraint,
                columns: Vec::new(),
                referenced_columns: Vec::new(),
            });
        }
        let key = referenced_by.last_mut().expect("pushed above");
        key.columns.push(column);
        key.referenced_columns.push(referenced_column);
    }

    // `SHOW CREATE TABLE` works for views too; the statement is the second column either way.
    let ddl = lease.conn().query_first::<mysql_async::Row, _>(format!("show create table {relation}")).await;
    let ddl = lease
        .check(ddl)
        .map_err(|e| query_error(&e))?
        .and_then(|row| row.get_opt::<String, _>(1).and_then(|r| r.ok()))
        .map(|sql| format!("{sql};"));

    Ok(TableStructure { columns, primary_key, indexes, foreign_keys, referenced_by, ddl })
}

/// What the structure view shows as a column's default: the default and the `extra` flags
/// (`auto_increment`, `on update CURRENT_TIMESTAMP`), or the generation expression.
fn column_default(default: Option<String>, extra: &str, generated: Option<String>) -> Option<String> {
    let lower = extra.to_ascii_lowercase();
    if let Some(expr) = generated.filter(|e| !e.is_empty() && lower.contains("generated") && !lower.contains("default_generated")) {
        let storage = if lower.contains("stored") { "STORED" } else { "VIRTUAL" };
        return Some(format!("GENERATED ALWAYS AS ({expr}) {storage}"));
    }
    let extra = extra.replace("DEFAULT_GENERATED", "");
    let parts: Vec<&str> = default.as_deref().into_iter().chain([extra.trim()]).filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

// MARK: Running SQL

/// Runs a script (several statements allowed) and returns the last result set, or, if no
/// statement returned rows, the affected-row count of the last one. Rows past `max_rows` are
/// counted, not kept: the stream is drained so later statements still run.
async fn run_script(conn: &mut Conn, sql: &str, max_rows: Option<u32>) -> std::result::Result<QueryResult, mysql_async::Error> {
    let max_rows = max_rows.map_or(usize::MAX, |m| m as usize);
    let mut stream = conn.query_iter(sql).await?;
    let mut last_rows: Option<QueryResult> = None;
    let mut last_affected: Option<u64> = None;

    // `columns()` is `Some` while a statement's result is pending (empty for INSERT/DDL…).
    // `is_empty()` can't be used: it is already true while the last OK result is pending.
    while let Some(columns) = stream.columns() {
        if columns.is_empty() {
            last_affected = Some(stream.affected_rows());
            stream.reduce((), |(), _: mysql_async::Row| ()).await?;
            continue;
        }
        let mut result = QueryResult {
            columns: columns.iter().map(|c| ColumnInfo {
                name: c.name_str().into_owned(),
                type_name: type_name(c),
                is_primary_key: false,
                is_nullable: true,
            }).collect(),
            ..Default::default()
        };
        result = stream
            .reduce(result, |mut result, row: mysql_async::Row| {
                if result.rows.len() < max_rows {
                    let values = row.unwrap().into_iter().zip(columns.iter()).map(|(v, c)| decode(v, c)).collect();
                    result.rows.push(values);
                } else {
                    result.truncated = true;
                    *result.total_count.get_or_insert(max_rows as u64) += 1;
                }
                result
            })
            .await?;
        last_rows = Some(result);
    }
    // An error in a later statement arrives after the previous result: surface it.
    stream.drop_result().await?;
    Ok(last_rows.unwrap_or(QueryResult { rows_affected: last_affected, ..Default::default() }))
}

/// Runs a table page query (the last result set counts) and also returns the raw values at
/// `keys` in every row, for keyset cursors.
async fn run_page(
    conn: &mut Conn,
    sql: &str,
    keys: &[usize],
) -> std::result::Result<(QueryResult, Vec<Vec<CursorValue>>), mysql_async::Error> {
    let mut stream = conn.query_iter(sql).await?;
    let mut last = (QueryResult::default(), Vec::new());
    while let Some(columns) = stream.columns() {
        if columns.is_empty() {
            stream.reduce((), |(), _: mysql_async::Row| ()).await?;
            continue;
        }
        last = stream
            .reduce((QueryResult::default(), Vec::new()), |(mut result, mut key_rows), row: mysql_async::Row| {
                let values = row.unwrap();
                if !keys.is_empty() {
                    key_rows.push(keys.iter().map(|&i| key_value(values.get(i).cloned())).collect::<Vec<_>>());
                }
                result.rows.push(values.into_iter().zip(columns.iter()).map(|(v, c)| decode(v, c)).collect());
                (result, key_rows)
            })
            .await?;
    }
    stream.drop_result().await?;
    Ok(last)
}

fn key_value(value: Option<mysql_async::Value>) -> CursorValue {
    use mysql_async::Value as V;
    match value {
        None | Some(V::NULL) => CursorValue::Null,
        Some(V::Bytes(b)) => String::from_utf8(b).map_or_else(|e| CursorValue::Bytes(e.into_bytes()), CursorValue::Text),
        Some(V::Int(i)) => CursorValue::Int(i),
        Some(other) => CursorValue::Text(other.as_sql(true)),
    }
}

/// Turns a text-protocol value into a typed `Value` using the column metadata.
fn decode(value: mysql_async::Value, column: &Column) -> Value {
    use mysql_async::Value as V;
    let bytes = match value {
        V::NULL => return Value::Null,
        V::Bytes(b) => b,
        // The text protocol only sends bytes; keep the binary-protocol cases sensible anyway.
        V::Int(i) => return Value::Int(i),
        V::UInt(u) => return i64::try_from(u).map_or_else(|_| Value::Decimal(u.to_string()), Value::Int),
        V::Float(f) => return Value::Float(f.into()),
        V::Double(f) => return Value::Float(f),
        other => return Value::Text(other.as_sql(true)),
    };
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    match column.column_type() {
        // TINYINT(1) is MySQL's BOOLEAN.
        ColumnType::MYSQL_TYPE_TINY if column.column_length() == 1 && !column.flags().contains(ColumnFlags::UNSIGNED_FLAG) => {
            match bytes.as_slice() {
                b"0" => Value::Bool(false),
                b"1" => Value::Bool(true),
                _ => Value::Text(text()),
            }
        }
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG
        | ColumnType::MYSQL_TYPE_YEAR => {
            // BIGINT UNSIGNED above i64::MAX stays exact as a decimal.
            let text = text();
            text.parse().map_or(Value::Decimal(text), Value::Int)
        }
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
            let text = text();
            text.parse().map_or(Value::Text(text), Value::Float)
        }
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => Value::Decimal(text()),
        ColumnType::MYSQL_TYPE_BIT => {
            // Sent as big-endian bytes, e.g. b'101' → [0x05].
            if bytes.len() <= 8 {
                Value::Int(bytes.iter().fold(0i64, |acc, b| (acc << 8) | i64::from(*b)))
            } else {
                Value::Text(hex_preview(&bytes))
            }
        }
        ColumnType::MYSQL_TYPE_JSON => Value::Text(text()),
        ColumnType::MYSQL_TYPE_GEOMETRY => Value::Text(hex_preview(&bytes)),
        _ if column.character_set() == BINARY_CHARSET && is_string_type(column.column_type()) => {
            Value::Text(hex_preview(&bytes))
        }
        _ => Value::Text(text()),
    }
}

fn is_string_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::MYSQL_TYPE_STRING
            | ColumnType::MYSQL_TYPE_VAR_STRING
            | ColumnType::MYSQL_TYPE_VARCHAR
            | ColumnType::MYSQL_TYPE_BLOB
            | ColumnType::MYSQL_TYPE_TINY_BLOB
            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
            | ColumnType::MYSQL_TYPE_LONG_BLOB
    )
}

/// SQL-ish names for script result columns (tables use `information_schema.columns.column_type`).
fn type_name(column: &Column) -> String {
    let binary = column.character_set() == BINARY_CHARSET;
    let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let base = match column.column_type() {
        ColumnType::MYSQL_TYPE_TINY if column.column_length() == 1 && !unsigned => "tinyint(1)",
        ColumnType::MYSQL_TYPE_TINY => "tinyint",
        ColumnType::MYSQL_TYPE_SHORT => "smallint",
        ColumnType::MYSQL_TYPE_INT24 => "mediumint",
        ColumnType::MYSQL_TYPE_LONG => "int",
        ColumnType::MYSQL_TYPE_LONGLONG => "bigint",
        ColumnType::MYSQL_TYPE_FLOAT => "float",
        ColumnType::MYSQL_TYPE_DOUBLE => "double",
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => "decimal",
        ColumnType::MYSQL_TYPE_YEAR => "year",
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date",
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "time",
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "datetime",
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        ColumnType::MYSQL_TYPE_BIT => "bit",
        ColumnType::MYSQL_TYPE_JSON => "json",
        ColumnType::MYSQL_TYPE_ENUM => "enum",
        ColumnType::MYSQL_TYPE_SET => "set",
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry",
        ColumnType::MYSQL_TYPE_STRING if binary => "binary",
        ColumnType::MYSQL_TYPE_STRING => "char",
        ColumnType::MYSQL_TYPE_VAR_STRING | ColumnType::MYSQL_TYPE_VARCHAR if binary => "varbinary",
        ColumnType::MYSQL_TYPE_VAR_STRING | ColumnType::MYSQL_TYPE_VARCHAR => "varchar",
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
            if binary => "blob",
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => "text",
        ColumnType::MYSQL_TYPE_NULL => "null",
        _ => "",
    };
    if unsigned && !base.is_empty() { format!("{base} unsigned") } else { base.into() }
}

// MARK: Errors

/// `ERROR 1064 (42000): You have an error in your SQL syntax; … at line 1`, like the mysql client.
pub(crate) fn query_error(e: &mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(s) if s.code == ER_QUERY_INTERRUPTED => Error::Cancelled,
        mysql_async::Error::Server(s) => Error::Query(format!("ERROR {} ({}): {}", s.code, s.state, s.message)),
        mysql_async::Error::Io(_) | mysql_async::Error::Driver(_) => Error::ConnectionFailed(error_chain(e)),
        _ => Error::Query(error_chain(e)),
    }
}

/// Server-reported startup errors (bad password, unknown database…) without the wrapper text.
fn connect_error(e: &mysql_async::Error) -> String {
    match e {
        mysql_async::Error::Server(s) => s.message.clone(),
        _ => error_chain(e),
    }
}
