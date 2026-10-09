//! Hardcoded sample connections, schemas and data so frontends can be built before real drivers exist.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;

use crate::driver::{Driver, Error, Result};
use crate::model::*;

/// Connections that simulate a failure (shows the warning state in the UI).
pub const UNREACHABLE: &[&str] = &["prod-replica"];

/// The sample connection backed by the real dev Postgres (`scripts/dev-db.sh up postgres`).
pub const DEV_DATABASE: &str = "local-pg";
/// The sample connection backed by the real dev MySQL (`scripts/dev-db.sh up mysql`).
pub const DEV_MYSQL: &str = "local-mysql";
/// The sample connection backed by the dev SQLite file (`scripts/dev-db.sh up sqlite`).
pub const DEV_SQLITE: &str = "local-sqlite";
/// The sample connection backed by the dev libSQL server (`scripts/dev-db.sh up libsql`).
pub const DEV_LIBSQL: &str = "local-libsql";
/// The sample connection backed by the real dev SQL Server (`scripts/dev-db.sh up sqlserver`).
pub const DEV_SQLSERVER: &str = "local-sqlserver";

/// Sample connections served by [`MockDriver`] (everything except the dev databases).
pub fn is_mock(config: &ConnectionConfig) -> bool {
    ![DEV_DATABASE, DEV_MYSQL, DEV_SQLITE, DEV_LIBSQL, DEV_SQLSERVER].contains(&config.id.as_str())
        && connections().iter().any(|c| c.id == config.id)
}

/// `dev/sqlite/app.db` in this checkout (sample connections are a development aid).
pub fn dev_sqlite_path() -> String {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(|p| p.parent());
    repo.map(|r| r.join("dev/sqlite/app.db").display().to_string()).unwrap_or_default()
}

pub fn connections() -> Vec<ConnectionConfig> {
    #[allow(clippy::too_many_arguments)]
    fn conn(
        id: &str, name: &str, group: &str, kind: DatabaseKind, host: &str, port: Option<u16>, db: &str,
        user: Option<&str>,
    ) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: name.into(),
            group: group.into(),
            kind,
            host: host.into(),
            port,
            database: db.into(),
            user: user.map(Into::into),
            password: None,
            ssl_mode: SslMode::default(),
            show_all_databases: kind != DatabaseKind::Sqlite,
            ssh: None,
        }
    }
    use DatabaseKind::*;
    vec![
        ConnectionConfig {
            password: Some("postgres".into()),
            ..conn(DEV_DATABASE, "app_dev", "Local", Postgres, "localhost", Some(54329), "app_dev", Some("postgres"))
        },
        // No database: every database on the server is listed as a schema.
        ConnectionConfig {
            password: Some("mysql".into()),
            ..conn(DEV_MYSQL, "mysql_dev", "Local", Mysql, "localhost", Some(33069), "", Some("root"))
        },
        conn(DEV_SQLITE, "app.db", "Local", Sqlite, "", None, &dev_sqlite_path(), None),
        // Token from `dev/libsql/dev_token` (dev-only key pair).
        ConnectionConfig {
            password: Some(include_str!("../../../dev/libsql/dev_token").trim().into()),
            ssl_mode: SslMode::Disable,
            show_all_databases: false,
            ..conn(DEV_LIBSQL, "libsql_dev", "Local", Libsql, "localhost", Some(18080), "", None)
        },
        // Self-signed certificate: "require" encrypts without verifying it.
        ConnectionConfig {
            password: Some("Dbear_dev1".into()),
            ssl_mode: SslMode::Require,
            ..conn(DEV_SQLSERVER, "mssql_dev", "Local", SqlServer, "localhost", Some(14339), "app_dev", Some("sa"))
        },
        conn("staging-pg", "app_staging", "Staging", Postgres, "staging-db.internal", Some(5432), "app", Some("readonly")),
        conn("prod-pg", "app_production", "Production", Postgres, "prod-db.internal", Some(5432), "app", Some("readonly")),
        conn("prod-replica", "app_replica", "Production", Postgres, "replica-db.internal", Some(5432), "app", Some("readonly")),
    ]
}

struct TableSpec {
    name: &'static str,
    kind: TableKind,
    rows: u64,
    columns: Vec<ColumnInfo>,
}

type Specs = BTreeMap<&'static str, Vec<TableSpec>>;

fn col(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo { name: name.into(), type_name: ty.into(), is_primary_key: false, is_nullable: false }
}
fn pk(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo { is_primary_key: true, ..col(name, ty) }
}
fn nullable(name: &str, ty: &str) -> ColumnInfo {
    ColumnInfo { is_nullable: true, ..col(name, ty) }
}
fn table(name: &'static str, rows: u64, columns: Vec<ColumnInfo>) -> TableSpec {
    TableSpec { name, kind: TableKind::Table, rows, columns }
}
fn view(name: &'static str, rows: u64, columns: Vec<ColumnInfo>) -> TableSpec {
    TableSpec { name, kind: TableKind::View, rows, columns }
}

fn postgres_specs() -> &'static Specs {
    static S: OnceLock<Specs> = OnceLock::new();
    S.get_or_init(|| {
        BTreeMap::from([
            ("public", vec![
                table("users", 248, vec![pk("id", "bigint"), col("name", "text"), col("email", "text"),
                    col("is_admin", "boolean"), nullable("last_login_at", "timestamptz"), col("created_at", "timestamptz")]),
                table("orders", 1_204, vec![pk("id", "bigint"), col("user_id", "bigint"), col("status", "text"),
                    col("total", "numeric"), nullable("notes", "text"), col("created_at", "timestamptz")]),
                table("products", 86, vec![pk("id", "bigint"), col("sku", "text"), col("name", "text"),
                    col("price", "numeric"), col("in_stock", "boolean")]),
                table("sessions", 512, vec![pk("id", "uuid"), col("user_id", "bigint"), col("ip", "inet"),
                    col("expires_at", "timestamptz")]),
                view("active_users", 37, vec![col("id", "bigint"), col("name", "text"), col("email", "text"),
                    col("last_login_at", "timestamptz")]),
            ]),
            ("billing", vec![
                table("invoices", 930, vec![pk("id", "bigint"), col("order_id", "bigint"), col("amount", "numeric"),
                    col("paid", "boolean"), col("due_at", "timestamptz")]),
                table("payments", 874, vec![pk("id", "bigint"), col("invoice_id", "bigint"), col("provider", "text"),
                    col("amount", "numeric"), col("created_at", "timestamptz")]),
                table("subscriptions", 61, vec![pk("id", "bigint"), col("user_id", "bigint"), col("plan", "text"),
                    col("status", "text"), nullable("canceled_at", "timestamptz")]),
            ]),
            ("analytics", vec![
                table("events", 50_000, vec![pk("id", "bigint"), nullable("user_id", "bigint"), col("name", "text"),
                    col("path", "text"), col("created_at", "timestamptz")]),
                view("daily_signups", 90, vec![col("day", "date"), col("count", "bigint")]),
            ]),
        ])
    })
}

fn mysql_specs() -> &'static Specs {
    static S: OnceLock<Specs> = OnceLock::new();
    S.get_or_init(|| {
        BTreeMap::from([("wordpress", vec![
            table("wp_posts", 312, vec![pk("ID", "bigint"), col("post_title", "varchar"), col("status", "varchar"),
                col("post_author", "bigint"), col("created_at", "datetime")]),
            table("wp_users", 12, vec![pk("ID", "bigint"), col("user_login", "varchar"), col("email", "varchar"),
                col("created_at", "datetime")]),
            table("wp_options", 140, vec![pk("option_id", "bigint"), col("option_name", "varchar"),
                nullable("option_value", "longtext")]),
        ])])
    })
}

fn sqlite_specs() -> &'static Specs {
    static S: OnceLock<Specs> = OnceLock::new();
    S.get_or_init(|| {
        BTreeMap::from([("main", vec![
            table("notes", 57, vec![pk("id", "INTEGER"), col("title", "TEXT"), nullable("body", "TEXT"),
                col("pinned", "INTEGER"), col("created_at", "TEXT")]),
            table("tags", 9, vec![pk("id", "INTEGER"), col("name", "TEXT")]),
        ])])
    })
}

fn specs(kind: DatabaseKind) -> &'static Specs {
    match kind {
        DatabaseKind::Postgres => postgres_specs(),
        DatabaseKind::Mysql => mysql_specs(),
        DatabaseKind::Sqlite | DatabaseKind::Libsql => sqlite_specs(),
        DatabaseKind::SqlServer => postgres_specs(),
    }
}

// MARK: Deterministic fake values

const NAMES: &[&str] = &["Ada Lovelace", "Alan Turing", "Grace Hopper", "Linus Torvalds", "Barbara Liskov",
    "Ken Thompson", "Dennis Ritchie", "Margaret Hamilton", "Edsger Dijkstra", "Donald Knuth"];
const STATUSES: &[&str] = &["pending", "paid", "shipped", "delivered", "canceled", "refunded"];
const WORDS: &[&str] = &["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn value(column: &ColumnInfo, i: u64) -> Value {
    if column.is_nullable && i % 4 == 1 {
        return Value::Null;
    }
    let n = column.name.to_lowercase();
    let t = column.type_name.to_lowercase();
    let w = |k: u64| WORDS[(k % WORDS.len() as u64) as usize];
    let text = |s: String| Value::Text(s);

    if column.is_primary_key && t == "uuid" {
        return text(format!("{:08x}-4b1c-9e2a-{:012x}", i.wrapping_mul(2_654_435_761) & 0xffff_ffff, i * 7919));
    }
    if column.is_primary_key || n == "id" {
        return Value::Int(i as i64 + 1);
    }
    if n.ends_with("_id") || n == "post_author" {
        return Value::Int(((i * 37) % 250 + 1) as i64);
    }
    if t == "boolean" || n == "pinned" {
        return Value::Bool(!i.is_multiple_of(3));
    }
    if t == "numeric" {
        return Value::Decimal(format!("{:.2}", ((i * 1_733) % 50_000) as f64 / 100.0 + 4.99));
    }
    if t.contains("time") || t == "date" || n.ends_with("_at") || n == "day" {
        let (day, month) = (1 + i % 28, 1 + (i / 28) % 12);
        let (hour, minute) = ((i * 7) % 24, (i * 13) % 60);
        let date = format!("2025-{month:02}-{day:02}");
        return text(if t == "date" { date } else { format!("{date} {hour:02}:{minute:02}:00") });
    }
    if n.ends_with("email") {
        let name = NAMES[(i % NAMES.len() as u64) as usize].to_lowercase().replace(' ', ".");
        return text(format!("{name}{i}@example.com"));
    }
    if n == "option_name" {
        return text(format!("option_{}_{i}", w(i)));
    }
    if n == "post_title" || n == "title" {
        return text(format!("{} note #{}", capitalized(w(i)), i + 1));
    }
    if n.contains("name") || n == "user_login" {
        return text(NAMES[(i % NAMES.len() as u64) as usize].into());
    }
    match n.as_str() {
        "status" => text(STATUSES[(i % STATUSES.len() as u64) as usize].into()),
        "plan" => text(["free", "pro", "team"][(i % 3) as usize].into()),
        "provider" => text(["stripe", "paypal", "mercadopago"][(i % 3) as usize].into()),
        "sku" => text(format!("SKU-{:05}", i * 17)),
        "ip" => text(format!("10.0.{}.{}", i % 255, (i * 3) % 255)),
        "path" => text(format!("/{}/{}", w(i), w(i + 3))),
        _ if t == "bigint" || t == "integer" => Value::Int(((i * 31) % 1_000) as i64),
        _ => text(format!("{} {}", w(i), w(i * 5))),
    }
}

/// Sort order for sample values: numbers numerically, NULLs last (like Postgres' default).
fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let number = |v: &Value| match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.parse().ok(),
        _ => None,
    };
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => match (number(a), number(b)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            _ => a.display().cmp(&b.display()),
        },
    }
}

pub struct MockDriver {
    config: ConnectionConfig,
    connected: AtomicBool,
}

impl MockDriver {
    pub fn new(config: ConnectionConfig) -> Self {
        Self { config, connected: AtomicBool::new(false) }
    }

    async fn latency() {
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    fn find(&self, schema: Option<&str>, name: &str) -> Option<(&'static str, &'static TableSpec)> {
        let specs = specs(self.config.kind);
        // Unqualified names resolve against "public" first, like a default search_path.
        let mut order: Vec<_> = specs.iter().collect();
        order.sort_by_key(|(k, _)| (**k != "public", **k));
        order.into_iter().filter(|(k, _)| schema.is_none_or(|s| s == **k)).find_map(|(k, tables)| {
            tables.iter().find(|t| t.name == name).map(|t| (*k, t))
        })
    }
}

#[async_trait]
impl Driver for MockDriver {
    fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    async fn connect(&self) -> Result<()> {
        Self::latency().await;
        if UNREACHABLE.contains(&self.config.id.as_str()) {
            return Err(Error::ConnectionFailed(format!(
                "could not connect to server at \"{}\" (timeout)",
                self.config.host
            )));
        }
        self.connected.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.connect().await?;
        let db = self.config.default_database().to_string();
        let mut names = match self.config.kind {
            DatabaseKind::Postgres => vec![format!("{db}_test"), db, "postgres".into()],
            DatabaseKind::Mysql => vec![db, "shop".into()],
            DatabaseKind::Sqlite | DatabaseKind::Libsql => vec![db],
            DatabaseKind::SqlServer => vec![db, "master".into()],
        };
        names.retain(|n| !n.is_empty());
        names.sort();
        names.dedup();
        Ok(names)
    }

    async fn list_schemas(&self) -> Result<Vec<Schema>> {
        self.connect().await?;
        Ok(specs(self.config.kind)
            .iter()
            .map(|(name, tables)| Schema {
                name: (*name).into(),
                tables: tables
                    .iter()
                    .map(|t| TableInfo {
                        schema: (*name).into(),
                        name: t.name.into(),
                        kind: t.kind,
                        estimated_row_count: Some(t.rows),
                    })
                    .collect(),
            })
            .collect())
    }

    async fn list_columns(&self) -> Result<Vec<TableColumns>> {
        self.connect().await?;
        Ok(specs(self.config.kind)
            .iter()
            .flat_map(|(schema, tables)| {
                tables.iter().map(move |t| TableColumns {
                    schema: (*schema).into(),
                    table: t.name.into(),
                    columns: t.columns.clone(),
                })
            })
            .collect())
    }

    async fn fetch_rows(&self, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Result<QueryResult> {
        self.connect().await?;
        let (_, spec) = self
            .find(Some(&table.schema), &table.name)
            .ok_or_else(|| Error::TableNotFound(table.qualified_name()))?;
        if query.filter.is_some() {
            return Err(Error::Unsupported("filters need a real database (sample data can’t evaluate SQL)".into()));
        }
        let row = |i: u64| -> Vec<Value> { spec.columns.iter().map(|c| value(c, i)).collect() };
        let end = spec.rows.min(offset + limit as u64);
        let rows = if query.sort.is_empty() {
            (offset..end.max(offset)).map(row).collect()
        } else {
            // Sorting needs every row; sample tables are small enough to generate whole.
            let keys: Vec<(usize, bool)> = query
                .sort
                .iter()
                .map(|k| {
                    let index = spec.columns.iter().position(|c| c.name == k.column).ok_or_else(|| {
                        Error::Query(format!("Can’t sort by “{}”: no such column", k.column))
                    })?;
                    Ok((index, k.descending))
                })
                .collect::<Result<_>>()?;
            let mut all: Vec<Vec<Value>> = (0..spec.rows).map(row).collect();
            all.sort_by(|a, b| {
                keys.iter()
                    .map(|&(i, descending)| {
                        let order = compare(&a[i], &b[i]);
                        if descending { order.reverse() } else { order }
                    })
                    .find(|o| o.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            all.into_iter().skip(offset as usize).take(limit as usize).collect()
        };
        Ok(QueryResult {
            columns: spec.columns.clone(),
            rows,
            total_count: Some(spec.rows),
            ..Default::default()
        })
    }

    async fn describe_table(&self, table: &TableInfo) -> Result<TableStructure> {
        self.connect().await?;
        let (schema, spec) = self
            .find(Some(&table.schema), &table.name)
            .ok_or_else(|| Error::TableNotFound(table.qualified_name()))?;
        let dialect = crate::dialect::Dialect(self.config.kind);
        let primary_key: Vec<String> = spec.columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
        let relation = dialect.quote_relation(schema, spec.name);
        let ddl = if spec.kind == TableKind::View {
            format!("CREATE VIEW {relation} AS\nSELECT 1;")
        } else {
            let mut lines: Vec<String> = spec
                .columns
                .iter()
                .map(|c| {
                    let null = if c.is_nullable { "" } else { " NOT NULL" };
                    format!("    {} {}{null}", dialect.quote_ident(&c.name), c.type_name)
                })
                .collect();
            if !primary_key.is_empty() {
                let keys: Vec<String> = primary_key.iter().map(|k| dialect.quote_ident(k)).collect();
                lines.push(format!("    PRIMARY KEY ({})", keys.join(", ")));
            }
            format!("CREATE TABLE {relation} (\n{}\n);", lines.join(",\n"))
        };
        Ok(TableStructure {
            columns: spec
                .columns
                .iter()
                .map(|c| ColumnDetail {
                    name: c.name.clone(),
                    type_name: c.type_name.clone(),
                    is_nullable: c.is_nullable,
                    default_value: None,
                    is_primary_key: c.is_primary_key,
                    comment: None,
                })
                .collect(),
            indexes: if primary_key.is_empty() || spec.kind == TableKind::View {
                Vec::new()
            } else {
                vec![IndexInfo {
                    name: format!("{}_pkey", spec.name),
                    columns: primary_key.clone(),
                    is_unique: true,
                    is_primary: true,
                    definition: None,
                }]
            },
            primary_key,
            foreign_keys: Vec::new(),
            referenced_by: Vec::new(),
            ddl: Some(ddl),
        })
    }

    async fn apply(&self, _statements: &[crate::edit::EditStatement]) -> Result<u64> {
        Err(Error::Unsupported("sample data is read-only: edit a real database".into()))
    }

    /// Understands just enough SQL to be useful for UI work:
    /// `SELECT * | col, col FROM [schema.]table [LIMIT n]`.
    async fn execute(&self, sql: &str, max_rows: Option<u32>) -> Result<QueryResult> {
        self.connect().await?;

        static SELECT: OnceLock<Regex> = OnceLock::new();
        let re = SELECT.get_or_init(|| {
            Regex::new(r#"(?is)^select\s+(.+?)\s+from\s+"?(\w+)"?(?:\."?(\w+)"?)?(?:\s+limit\s+(\d+))?$"#).unwrap()
        });

        let statement = sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join(" ");
        let statement = statement.trim().trim_end_matches(';').trim();

        let caps = re.captures(statement).ok_or_else(|| {
            Error::Unsupported("the mock driver only understands SELECT … FROM table [LIMIT n]".into())
        })?;
        let (schema, name) = match (caps.get(2), caps.get(3)) {
            (Some(s), Some(t)) => (Some(s.as_str()), t.as_str()),
            (Some(t), None) => (None, t.as_str()),
            _ => unreachable!(),
        };
        let (schema, spec) = self.find(schema, name).ok_or_else(|| {
            Error::TableNotFound(schema.map_or(name.to_string(), |s| format!("{s}.{name}")))
        })?;
        let wanted: u64 = caps.get(4).and_then(|m| m.as_str().parse().ok()).unwrap_or(spec.rows).min(spec.rows);
        let kept = max_rows.map_or(wanted, |m| wanted.min(m as u64));
        let mut full = self.fetch_rows(&TableInfo::new(schema, spec.name), &RowQuery::default(), kept as u32, 0).await?;
        full.truncated = kept < wanted;
        full.total_count = full.truncated.then_some(wanted);
        let origin = |c: &ColumnInfo| Some(ColumnOrigin { schema: schema.into(), table: spec.name.into(), column: c.name.clone() });
        full.origins = full.columns.iter().map(origin).collect();

        let select_list = caps[1].trim();
        if select_list == "*" {
            return Ok(full);
        }
        let indices = select_list
            .split(',')
            .map(|c| {
                let c = c.trim();
                full.columns
                    .iter()
                    .position(|col| col.name.eq_ignore_ascii_case(c))
                    .ok_or_else(|| Error::Query(format!("column \"{c}\" does not exist")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(QueryResult {
            columns: indices.iter().map(|&i| full.columns[i].clone()).collect(),
            rows: full.rows.iter().map(|r| indices.iter().map(|&i| r[i].clone()).collect()).collect(),
            origins: indices.iter().map(|&i| full.origins[i].clone()).collect(),
            ..full
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connection;

    /// Mock Postgres connection with the same sample schema the old app_dev mock had.
    fn app_dev() -> Connection {
        Connection::new(connections().into_iter().find(|c| c.id == "staging-pg").unwrap())
    }

    // Plain #[test] + a throwaway executor proves Connection works outside tokio (as from Swift/GPUI).
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn lists_databases_and_switches_with_with_database() {
        let config = connections().into_iter().find(|c| c.id == "staging-pg").unwrap();
        let dbs = block_on(Connection::new(config.clone()).list_databases()).unwrap();
        assert_eq!(dbs, ["app", "app_test", "postgres"]);
        let other = config.with_database("app_test");
        assert_eq!((other.id.as_str(), other.database.as_str()), ("staging-pg", "app_test"));
        assert!(!block_on(Connection::new(other).list_schemas()).unwrap().is_empty());
    }

    #[test]
    fn lists_schemas_sorted() {
        let schemas = block_on(app_dev().list_schemas()).unwrap();
        let names: Vec<_> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["analytics", "billing", "public"]);
    }

    #[test]
    fn fetches_rows_with_matching_column_count() {
        let result = block_on(app_dev().fetch_rows(TableInfo::new("public", "users"), 50, 0)).unwrap();
        assert_eq!(result.rows.len(), 50);
        assert!(result.rows.iter().all(|r| r.len() == result.columns.len()));
        assert_eq!(result.total_count, Some(248));
    }

    #[test]
    fn sorts_sample_rows_and_rejects_filters() {
        let users = TableInfo::new("public", "users");
        let query = RowQuery { sort: vec![SortKey { column: "id".into(), descending: true }], filter: None };
        let page = block_on(app_dev().fetch_rows_with(users.clone(), query, 3, 0)).unwrap();
        let ids: Vec<_> = page.rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(ids, [Value::Int(248), Value::Int(247), Value::Int(246)]);

        let filtered = RowQuery { filter: Some("id = 1".into()), ..Default::default() };
        assert!(matches!(block_on(app_dev().fetch_rows_with(users.clone(), filtered, 3, 0)), Err(Error::Unsupported(_))));
        // Blank filters are no filter at all.
        let blank = RowQuery { filter: Some("  ".into()), ..Default::default() };
        assert_eq!(block_on(app_dev().fetch_rows_with(users, blank, 3, 0)).unwrap().rows.len(), 3);
    }

    #[test]
    fn describes_sample_tables() {
        let s = block_on(app_dev().describe_table(TableInfo::new("public", "users"))).unwrap();
        assert_eq!(s.primary_key, ["id"]);
        assert_eq!(s.columns.len(), 6);
        assert!(s.ddl.unwrap().starts_with("CREATE TABLE \"public\".\"users\""));
    }

    #[test]
    fn pages_past_the_end_are_empty() {
        let result = block_on(app_dev().fetch_rows(TableInfo::new("public", "users"), 50, 1_000)).unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn unreachable_connection_fails() {
        let config = connections().into_iter().find(|c| UNREACHABLE.contains(&c.id.as_str())).unwrap();
        let err = block_on(Connection::new(config).list_schemas()).unwrap_err();
        assert!(matches!(err, Error::ConnectionFailed(_)));
    }

    #[test]
    fn executes_simple_select() {
        let result = block_on(app_dev().execute("-- comment\nselect id, email from users limit 5;".into())).unwrap();
        let cols: Vec<_> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, ["id", "email"]);
        assert_eq!(result.rows.len(), 5);
    }

    #[test]
    fn caps_script_rows() {
        let r = block_on(app_dev().execute_limited("select * from users".into(), Some(100))).unwrap();
        assert_eq!((r.rows.len(), r.truncated, r.total_count), (100, true, Some(248)));
        let r = block_on(app_dev().execute_limited("select * from users limit 10".into(), Some(100))).unwrap();
        assert_eq!((r.rows.len(), r.truncated, r.total_count), (10, false, None));
    }

    #[test]
    fn rejects_unsupported_sql() {
        let err = block_on(app_dev().execute("delete from users".into())).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    #[test]
    fn tracks_connection_state() {
        let conn = app_dev();
        assert!(!block_on(conn.is_connected()));
        block_on(conn.list_schemas()).unwrap();
        assert!(block_on(conn.is_connected()));
        block_on(conn.disconnect());
        assert!(!block_on(conn.is_connected()));
    }

    #[test]
    fn only_the_dev_database_is_real() {
        let real: Vec<_> = connections().into_iter().filter(|c| !is_mock(c)).map(|c| c.id).collect();
        assert_eq!(real, [DEV_DATABASE, DEV_MYSQL, DEV_SQLITE, DEV_LIBSQL, DEV_SQLSERVER]);
        assert!(format!("{:?}", connections()[0]).contains("password: Some(\"•••\")"));
    }

    #[test]
    fn summary_formats() {
        let c = &connections();
        assert_eq!(c[0].summary(), "PostgreSQL · localhost:54329/app_dev");
        assert_eq!(c[1].summary(), "MySQL · localhost:33069");
        assert!(c[2].summary().starts_with("SQLite · /") && c[2].summary().ends_with("dev/sqlite/app.db"));
        assert_eq!(c[3].summary(), "Turso · localhost:18080");
    }
}
