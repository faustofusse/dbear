//! Plain data types shared by every frontend.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseKind {
    Postgres,
    Mysql,
    Sqlite,
    /// Turso / libSQL server (`sqld`) over Hrana HTTP; SQLite-compatible SQL.
    Libsql,
    /// Microsoft SQL Server (and Azure SQL), over TDS.
    SqlServer,
}

impl DatabaseKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Sqlite => "SQLite",
            Self::Libsql => "Turso",
            Self::SqlServer => "SQL Server",
        }
    }

    /// The server's standard port (`None` for file databases).
    pub fn default_port(self) -> Option<u16> {
        match self {
            Self::Postgres => Some(5432),
            Self::Mysql => Some(3306),
            Self::Sqlite | Self::Libsql => None,
            Self::SqlServer => Some(1433),
        }
    }

    /// Speaks SQLite's SQL (quoting, catalog, editing rules): SQLite files and Turso / libSQL.
    pub fn is_sqlite_family(self) -> bool {
        matches!(self, Self::Sqlite | Self::Libsql)
    }
}

/// TLS behaviour, named after libpq's `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Plain TCP.
    Disable,
    /// TLS if the server supports it, without verifying its certificate.
    #[default]
    Prefer,
    /// TLS required, certificate not verified (same as libpq).
    Require,
    /// TLS required, certificate chain and hostname verified.
    VerifyFull,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub group: String,
    pub kind: DatabaseKind,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: Option<String>,
    /// Supplied by the frontend from the platform keychain; never persisted by the core.
    pub password: Option<String>,
    pub ssl_mode: SslMode,
    /// Offer every database on the server, not just `database`, to switch between (Postgres, MySQL).
    /// `database` stays the one opened first.
    pub show_all_databases: bool,
    /// Reach the server through an SSH server (Postgres, MySQL, SQL Server). `host` and `port` are
    /// then as seen from the SSH server.
    pub ssh: Option<SshTunnel>,
}

/// How to sign in to an SSH server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuth {
    #[default]
    Password,
    /// A private key file (`SshTunnel::key_path`), with an optional passphrase.
    PrivateKey,
    /// The keys of the running SSH agent (`SSH_AUTH_SOCK`; Pageant or OpenSSH's agent on Windows).
    Agent,
}

/// An SSH server the database is reached through (port forwarding, like `ssh -L`).
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct SshTunnel {
    pub host: String,
    /// `None`: 22.
    pub port: Option<u16>,
    pub user: String,
    pub auth: SshAuth,
    /// The private key for [`SshAuth::PrivateKey`] (`~` is expanded).
    pub key_path: String,
    /// The SSH password, or the key's passphrase. Supplied by the frontend from the platform
    /// keychain (see `secrets::ssh_account`); never persisted by the core.
    pub secret: Option<String>,
    /// Set by the core while a tunnel is open: the local port it listens on. Leave `None`.
    pub forwarded_port: Option<u16>,
}

impl SshTunnel {
    pub const DEFAULT_PORT: u16 = 22;

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(Self::DEFAULT_PORT)
    }
}

impl std::fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnel")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("key_path", &self.key_path)
            .field("secret", &self.secret.as_ref().map(|_| "•••"))
            .field("forwarded_port", &self.forwarded_port)
            .finish()
    }
}

impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "•••"))
            .field("ssl_mode", &self.ssl_mode)
            .field("show_all_databases", &self.show_all_databases)
            .field("ssh", &self.ssh)
            .finish()
    }
}

impl ConnectionConfig {
    /// Whether the server's other databases can be switched to, each opened with its own
    /// session (Postgres, MySQL, SQL Server). SQLite files have none.
    pub fn supports_multiple_databases(&self) -> bool {
        matches!(self.kind, DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::SqlServer)
    }

    /// The same connection pointed at another database on the server.
    /// Whether this kind of database can be reached through an SSH tunnel (TCP servers; not SQLite
    /// files, nor Turso, which is reached over HTTPS).
    pub fn supports_ssh(&self) -> bool {
        matches!(self.kind, DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::SqlServer)
    }

    /// The local port of the open SSH tunnel to connect to instead of `host:port` (drivers keep
    /// `host` for TLS: the certificate is the server's).
    pub(crate) fn tunneled_port(&self) -> Option<u16> {
        self.ssh.as_ref().and_then(|s| s.forwarded_port)
    }

    pub fn with_database(&self, database: &str) -> Self {
        Self { database: database.into(), ..self.clone() }
    }

    /// The database actually opened. `database` is optional for servers: Postgres then uses its
    /// `postgres` maintenance database (present on virtually every server), SQL Server `master`,
    /// MySQL needs none.
    pub fn default_database(&self) -> &str {
        let configured = self.database.trim();
        match self.kind {
            DatabaseKind::Postgres if configured.is_empty() => "postgres",
            DatabaseKind::SqlServer if configured.is_empty() => "master",
            _ => configured,
        }
    }

    /// Name used when the user leaves it empty: the database (file name for SQLite), else the host.
    pub fn default_name(&self) -> String {
        let database = self.database.trim();
        let host = self.host.trim();
        match self.kind {
            DatabaseKind::Sqlite => crate::paths::file_name(database).to_string(),
            // `mydb-org.aws-us-east-1.turso.io` → `mydb-org`.
            DatabaseKind::Libsql => host.split('.').next().unwrap_or_default().to_string(),
            _ if !database.is_empty() => database.to_string(),
            _ => host.to_string(),
        }
    }

    /// e.g. "PostgreSQL · localhost:5432/app_dev", or "PostgreSQL · localhost:5432" without a database;
    /// "… via bastion.example.com" through an SSH server.
    pub fn summary(&self) -> String {
        let summary = self.address_summary();
        match self.ssh.as_ref().filter(|_| self.supports_ssh()) {
            Some(ssh) if !ssh.host.trim().is_empty() => format!("{summary} via {}", ssh.host.trim()),
            _ => summary,
        }
    }

    fn address_summary(&self) -> String {
        let kind = self.kind.display_name();
        if self.kind == DatabaseKind::Sqlite {
            return format!("{kind} · {}", self.database);
        }
        if self.kind == DatabaseKind::Libsql {
            return match self.port {
                Some(port) => format!("{kind} · {}:{port}", self.host),
                None => format!("{kind} · {}", self.host),
            };
        }
        let address = match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        };
        if self.database.trim().is_empty() {
            format!("{kind} · {address}")
        } else {
            format!("{kind} · {address}/{}", self.database)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub estimated_row_count: Option<u64>,
}

impl TableInfo {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { schema: schema.into(), name: name.into(), kind: TableKind::Table, estimated_row_count: None }
    }

    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Schema {
    pub name: String,
    pub tables: Vec<TableInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub is_primary_key: bool,
    pub is_nullable: bool,
}

/// Columns of one table or view, for SQL completion (`Driver::list_columns`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableColumns {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

/// One `ORDER BY` term for table browsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

/// How to browse a table: user sort and a `WHERE` filter. The default is the table's natural
/// order (primary key, else physical order) with no filter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct RowQuery {
    /// Applied in order; the driver appends the primary key (or row id) so paging stays stable.
    pub sort: Vec<SortKey>,
    /// A raw SQL boolean expression, e.g. `status = 'paid' and total > 10`. Must be a single
    /// expression: a top-level `;` is rejected (see [`crate::dialect::normalize_filter`]).
    pub filter: Option<String>,
}

/// A column as described in the structure view.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnDetail {
    pub name: String,
    pub type_name: String,
    pub is_nullable: bool,
    /// Default or generation expression as the database spells it (`nextval(…)`, `CURRENT_TIMESTAMP`,
    /// `auto_increment`, `GENERATED ALWAYS AS IDENTITY`…).
    pub default_value: Option<String>,
    pub is_primary_key: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IndexInfo {
    pub name: String,
    /// Column names in index order; expressions are shown as written (`lower(email)`).
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Full `CREATE INDEX` statement when the database keeps one (Postgres, SQLite).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForeignKeyInfo {
    /// Constraint name (empty for SQLite, which doesn't name them).
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    /// Empty when SQLite references the parent's primary key implicitly.
    pub referenced_columns: Vec<String>,
    /// `NO ACTION`, `CASCADE`, `SET NULL`…
    pub on_update: String,
    pub on_delete: String,
}

/// A foreign key in another table that points at this one: rows of `schema.table` whose `columns`
/// hold this table's `referenced_columns` belong to that row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReferencingKey {
    /// The table holding the key.
    pub schema: String,
    pub table: String,
    /// Constraint name (empty for SQLite).
    pub name: String,
    /// The key's columns, in `table`.
    pub columns: Vec<String>,
    /// The columns of this table they point at; empty when SQLite references the primary key implicitly.
    pub referenced_columns: Vec<String>,
}

/// Everything the structure view shows for a table or view (`Driver::describe_table`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TableStructure {
    pub columns: Vec<ColumnDetail>,
    /// Primary key columns in key order (empty for views and keyless tables).
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Foreign keys of other tables (in the same database) that point at this one.
    pub referenced_by: Vec<ReferencingKey>,
    /// `CREATE TABLE` / `CREATE VIEW` (plus indexes) as SQL, when it can be produced.
    pub ddl: Option<String>,
}

/// A single cell. Drivers decode wire types into one of these.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Exact numerics (NUMERIC/DECIMAL) kept as text to avoid precision loss.
    Decimal(String),
    Text(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Canonical display string. Frontends may format differently, but this is the reference.
    pub fn display(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Decimal(s) | Self::Text(s) => s.clone(),
        }
    }
}

/// One page of rows. Rows are sent in pages, never cell by cell.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    /// Total rows available (e.g. table size), when known. For a truncated script result,
    /// the number of rows the statement actually returned.
    pub total_count: Option<u64>,
    /// `rows` stops at the requested row limit; the statement returned more.
    pub truncated: bool,
    /// For statements that return no rows (INSERT/UPDATE/DDL…): rows affected, as reported by the server.
    pub rows_affected: Option<u64>,
    /// Script results: where each column comes from, in `columns` order (`None` for expressions).
    /// Empty when the driver can't tell. See [`crate::results`].
    pub origins: Vec<Option<ColumnOrigin>>,
}

/// The table column a result column reads, unchanged (not an expression of it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnOrigin {
    pub schema: String,
    pub table: String,
    pub column: String,
}
