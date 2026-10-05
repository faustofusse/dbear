//! Users, roles and privileges: listing them (drivers' [`Driver::list_roles`] / [`Driver::list_grants`])
//! and changing them. Changes are described as data ([`AccessChange`]) and turned into SQL here, once
//! per dialect, so every frontend shows (and runs) the same statements.
//!
//! - Postgres: roles (users are roles that can log in), cluster-wide; privileges are per database,
//!   so [`Driver::list_grants`] reports the connection's current database.
//! - MySQL: accounts are `'user'@'host'`; roles (MySQL 8) are locked accounts granted to others.
//!   Privileges are server-wide (`*.*`), per database (`db.*`) or per table.
//!
//! Names and passwords are written as quoted identifiers / literals; privilege names are checked
//! against a keyword pattern, so nothing user-typed reaches the SQL unquoted.
//!
//! [`Driver::list_roles`]: crate::Driver::list_roles
//! [`Driver::list_grants`]: crate::Driver::list_grants

use std::collections::BTreeSet;

use crate::dialect::Dialect;
use crate::driver::{Error, Result};
use crate::model::DatabaseKind;

/// A role (Postgres) or account (MySQL, where `host` is set).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleRef {
    pub name: String,
    pub host: Option<String>,
}

impl RoleRef {
    pub fn new(name: impl Into<String>, host: Option<String>) -> Self {
        Self { name: name.into(), host }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    /// MySQL account host (`%`, `localhost`…); `None` for Postgres.
    pub host: Option<String>,
    /// Postgres `LOGIN`; MySQL: the account isn't locked. Roles that can't log in are groups.
    pub can_login: bool,
    pub is_superuser: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    /// Built in (`pg_*`, `mysql.sys`…): listed, but normally hidden.
    pub is_system: bool,
    /// Maximum concurrent connections; `None` = unlimited.
    pub connection_limit: Option<u32>,
    /// Postgres: the password stops working after this (as the server prints it).
    pub valid_until: Option<String>,
    /// Roles this one is a member of (inherits privileges from).
    pub member_of: Vec<RoleRef>,
    pub comment: Option<String>,
}

impl Role {
    pub fn reference(&self) -> RoleRef {
        RoleRef::new(&self.name, self.host.clone())
    }
}

/// A role as it should be: for [`AccessChange::CreateRole`] and [`AccessChange::AlterRole`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleSpec {
    pub name: String,
    pub host: Option<String>,
    /// New password; `None` (or empty) leaves it unset / unchanged.
    pub password: Option<String>,
    pub can_login: bool,
    pub is_superuser: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    pub connection_limit: Option<u32>,
    /// Postgres timestamp (`2026-12-31`); `None` = never expires.
    pub valid_until: Option<String>,
    pub member_of: Vec<RoleRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GrantObjectKind {
    Server,
    Database,
    Schema,
    Table,
    Sequence,
    AllTables,
    AllSequences,
}

/// What a privilege is on. For MySQL, `Table::schema` is the database.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GrantObject {
    /// MySQL `*.*`.
    Server,
    Database { name: String },
    Schema { name: String },
    /// Tables, views and other relations.
    Table { schema: String, name: String },
    Sequence { schema: String, name: String },
    /// Postgres `ALL TABLES IN SCHEMA`: applied to every table (and view) there now.
    AllTables { schema: String },
    /// Postgres `ALL SEQUENCES IN SCHEMA`.
    AllSequences { schema: String },
}

impl GrantObject {
    pub fn kind(&self) -> GrantObjectKind {
        match self {
            Self::Server => GrantObjectKind::Server,
            Self::Database { .. } => GrantObjectKind::Database,
            Self::Schema { .. } => GrantObjectKind::Schema,
            Self::Table { .. } => GrantObjectKind::Table,
            Self::Sequence { .. } => GrantObjectKind::Sequence,
            Self::AllTables { .. } => GrantObjectKind::AllTables,
            Self::AllSequences { .. } => GrantObjectKind::AllSequences,
        }
    }
}

/// One privilege a role holds on an object (directly, not through membership).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Grant {
    pub object: GrantObject,
    /// Upper case, as the server names it: `SELECT`, `CREATE TEMPORARY TABLES`…
    pub privilege: String,
    /// Held `WITH GRANT OPTION`: the role may grant it to others.
    pub grantable: bool,
}

/// The privileges a role holds on one object, before or after an edit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrivilegeSet {
    pub privileges: Vec<String>,
    /// All of them `WITH GRANT OPTION`.
    pub grantable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessChange {
    CreateRole(RoleSpec),
    /// `role` is the role as listed; `spec` what it should become (renames included).
    AlterRole { role: Role, spec: RoleSpec },
    DropRole(RoleRef),
    /// Grants what's in `after` but not `before`, revokes the reverse.
    SetPrivileges { role: RoleRef, object: GrantObject, before: PrivilegeSet, after: PrivilegeSet },
    /// Gives `role` a level in `context.database`, replacing what it had there. Postgres statements
    /// must run in that database (`Connection::apply_access` connects to it).
    SetDatabaseLevel { role: RoleRef, context: DatabaseLevelContext, level: DatabaseLevel },
}

impl AccessChange {
    /// The database the statements must run in, if not the connection's own (Postgres levels).
    pub fn database(&self, kind: DatabaseKind) -> Option<&str> {
        match self {
            Self::SetDatabaseLevel { context, .. } if kind == DatabaseKind::Postgres => Some(&context.database),
            _ => None,
        }
    }
}

/// A role's privileges on one database of the server (every database is listed, for an access editor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseAccess {
    pub database: String,
    /// Granted directly to the role on the database itself (Postgres `CONNECT`…, MySQL `db.*`).
    pub privileges: PrivilegeSet,
    /// Postgres: `PUBLIC` may connect, so any role can (the default for new databases).
    pub everyone_can_connect: bool,
    /// The role owns the database: it has every privilege on it, implicitly.
    pub is_owner: bool,
    /// The level, as far as the database-level privileges tell: exact for MySQL; for Postgres a role
    /// with privileges on the database shows `Custom` until probed there (`Connection::database_level`).
    pub level: DatabaseLevel,
}

/// How much a role may do in one database, as offered in an access menu. Postgres levels cover the
/// database, its schemas, tables and sequences, and those created later (default privileges);
/// MySQL's are privileges on `db.*`, which already covers future tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DatabaseLevel {
    NoAccess,
    /// Postgres: `CONNECT` only, nothing inside.
    Connect,
    ReadOnly,
    ReadWrite,
    /// Read and write, plus creating objects (Postgres) or changing the schema (MySQL).
    SchemaChanges,
    /// Privileges that match no level: shown, never applied.
    Custom,
}

impl DatabaseLevel {
    pub fn title(self) -> &'static str {
        match self {
            Self::NoAccess => "No access",
            Self::Connect => "Connect only",
            Self::ReadOnly => "Read only",
            Self::ReadWrite => "Read and write",
            Self::SchemaChanges => "Schema changes",
            Self::Custom => "Custom",
        }
    }

    /// What the level allows, in a sentence.
    pub fn summary(self, kind: DatabaseKind) -> &'static str {
        match (self, kind) {
            (Self::NoAccess, _) => "No privileges in the database.",
            (Self::Connect, _) => "Can connect, but not read any table.",
            (Self::ReadOnly, DatabaseKind::Postgres) => "Can read every table, view and sequence, including ones created later.",
            (Self::ReadOnly, _) => "Can read every table and view.",
            (Self::ReadWrite, DatabaseKind::Postgres) => "Can read and change rows in every table, including ones created later.",
            (Self::ReadWrite, _) => "Can read and change rows, and run routines.",
            (Self::SchemaChanges, DatabaseKind::Postgres) => {
                "Read and write, plus creating tables and schemas (only owners can alter or drop a table)."
            }
            (Self::SchemaChanges, _) => "Read and write, plus creating, altering and dropping tables, views and routines.",
            (Self::Custom, _) => "Privileges that match no level.",
        }
    }
}

/// The levels to offer for `kind`, from least to most.
pub fn database_levels(kind: DatabaseKind) -> Vec<DatabaseLevel> {
    use DatabaseLevel::*;
    match kind {
        DatabaseKind::Postgres => vec![NoAccess, Connect, ReadOnly, ReadWrite, SchemaChanges],
        DatabaseKind::Mysql => vec![NoAccess, ReadOnly, ReadWrite, SchemaChanges],
        _ => Vec::new(),
    }
}

/// What a role has in one database, plus what's needed to change it (detected in that database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseLevelContext {
    pub database: String,
    /// The role's current level there.
    pub level: DatabaseLevel,
    /// Privileges on the database itself (MySQL: `db.*`).
    pub privileges: PrivilegeSet,
    /// Postgres: the schemas a level applies to (all but system ones).
    pub schemas: Vec<String>,
    /// Postgres: roles whose future tables, sequences and schemas get default privileges: those owning
    /// schemas or tables there, the database owner and the current user (only ones it may act for).
    pub owners: Vec<String>,
}

const MYSQL_READ: &[&str] = &["SELECT", "SHOW VIEW"];
const MYSQL_WRITE: &[&str] = &["SELECT", "SHOW VIEW", "INSERT", "UPDATE", "DELETE", "EXECUTE", "LOCK TABLES", "CREATE TEMPORARY TABLES"];

/// MySQL privileges on `db.*` for a level.
fn mysql_level_privileges(level: DatabaseLevel) -> &'static [&'static str] {
    match level {
        DatabaseLevel::ReadOnly => MYSQL_READ,
        DatabaseLevel::ReadWrite => MYSQL_WRITE,
        DatabaseLevel::SchemaChanges => privileges(DatabaseKind::Mysql, GrantObjectKind::Database),
        _ => &[],
    }
}

/// The MySQL level that is exactly `privileges` on `db.*`.
pub(crate) fn mysql_level(privileges: &[String]) -> DatabaseLevel {
    if privileges.is_empty() {
        return DatabaseLevel::NoAccess;
    }
    let have: BTreeSet<&str> = privileges.iter().map(String::as_str).collect();
    [DatabaseLevel::ReadOnly, DatabaseLevel::ReadWrite, DatabaseLevel::SchemaChanges]
        .into_iter()
        .find(|&l| mysql_level_privileges(l).iter().copied().collect::<BTreeSet<_>>() == have)
        .unwrap_or(DatabaseLevel::Custom)
}

/// A role's privileges inside a Postgres database, counted (see `postgres::database_level`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PgLevelFacts {
    pub database: Vec<String>,
    pub schemas: i64,
    pub schemas_usage: i64,
    pub schemas_create: i64,
    /// Tables, views, materialized views and foreign tables.
    pub relations: i64,
    pub relations_select: i64,
    /// Ones that can be written to (not views).
    pub writable: i64,
    pub writable_write: i64,
    pub writable_ddl: i64,
    pub any_write: i64,
    pub any_ddl: i64,
    /// Default privileges the role gets on future tables.
    pub default_table: Vec<String>,
}

const PG_TABLE_READ: &[&str] = &["SELECT"];
const PG_TABLE_WRITE: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE"];
const PG_TABLE_ALL: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];

/// The level matching what a role holds in a Postgres database, or `Custom`.
pub(crate) fn pg_level(f: &PgLevelFacts) -> DatabaseLevel {
    let db: BTreeSet<&str> = f.database.iter().map(String::as_str).collect();
    let defaults: BTreeSet<&str> = f.default_table.iter().map(String::as_str).collect();
    let nothing_inside = f.schemas_usage == 0 && f.schemas_create == 0 && f.relations_select == 0 && f.any_write == 0 && f.any_ddl == 0 && defaults.is_empty();
    if db.is_empty() && nothing_inside {
        return DatabaseLevel::NoAccess;
    }
    if db == BTreeSet::from(["CONNECT"]) && nothing_inside {
        return DatabaseLevel::Connect;
    }
    let defaults_have = |p: &[&str]| p.iter().all(|x| defaults.contains(x));
    // With no tables yet, the default privileges tell the levels apart.
    let reads = if f.relations > 0 { f.relations_select == f.relations } else { defaults_have(PG_TABLE_READ) };
    let writes = if f.writable > 0 { f.writable_write == f.writable } else { defaults_have(PG_TABLE_WRITE) };
    let ddl = if f.writable > 0 { f.writable_ddl == f.writable } else { defaults_have(PG_TABLE_ALL) };
    let usage = f.schemas_usage == f.schemas;
    if !(db.contains("CONNECT") && usage && reads) {
        return DatabaseLevel::Custom;
    }
    if db == BTreeSet::from(["CONNECT", "CREATE", "TEMPORARY"]) && f.schemas_create == f.schemas && writes && ddl {
        return DatabaseLevel::SchemaChanges;
    }
    if db.len() != 1 || f.schemas_create > 0 || f.any_ddl > 0 || defaults_have(&["TRUNCATE"]) {
        return DatabaseLevel::Custom;
    }
    if writes && (f.writable == 0 || f.any_write == f.writable_write) {
        return DatabaseLevel::ReadWrite;
    }
    if f.any_write == 0 && !defaults.contains("INSERT") { DatabaseLevel::ReadOnly } else { DatabaseLevel::Custom }
}

/// A random password: `length` characters (at least 12) of letters, digits and URL-safe symbols,
/// with at least one of each, so strict password policies (MySQL's `validate_password`) accept it.
pub fn generate_password(length: usize) -> Result<String> {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGITS: &[u8] = b"23456789";
    const SYMBOLS: &[u8] = b"-_.~";
    let length = length.max(12);
    let all: Vec<u8> = [LOWER, UPPER, DIGITS, SYMBOLS].concat();
    // Unbiased picks: bytes past the largest multiple of the alphabet size are drawn again.
    let pick = |set: &[u8]| -> Result<u8> {
        let limit = 256 - 256 % set.len();
        loop {
            let mut byte = [0u8; 1];
            getrandom::fill(&mut byte).map_err(|e| Error::Internal(format!("no randomness: {e}")))?;
            if (byte[0] as usize) < limit {
                return Ok(set[byte[0] as usize % set.len()]);
            }
        }
    };
    loop {
        let password = (0..length).map(|_| pick(&all)).collect::<Result<Vec<u8>>>()?;
        let has = |set: &[u8]| password.iter().any(|c| set.contains(c));
        // Symbols at the ends get trimmed or mangled by some tools: keep letters or digits there.
        let ends_ok = !SYMBOLS.contains(&password[0]) && !SYMBOLS.contains(&password[length - 1]);
        if has(LOWER) && has(UPPER) && has(DIGITS) && has(SYMBOLS) && ends_ok {
            return Ok(String::from_utf8(password).expect("ASCII"));
        }
    }
}

/// A statement to run, and how to show it (passwords masked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessStatement {
    pub sql: String,
    pub display: String,
}

/// What a database supports, so frontends only offer what will work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessFeatures {
    /// Accounts have a host (`'user'@'host'`).
    pub hosts: bool,
    pub superuser: bool,
    pub create_db: bool,
    pub create_role: bool,
    pub valid_until: bool,
    pub connection_limit: bool,
    /// Roles can be granted to other roles.
    pub membership: bool,
    /// Privileges are listed for the connection's current database only (Postgres).
    pub grants_per_database: bool,
    /// Objects privileges can be granted on, in the order to offer them.
    pub object_kinds: Vec<GrantObjectKind>,
}

/// `None` if users can't be managed on `kind` (yet).
pub fn features(kind: DatabaseKind) -> Option<AccessFeatures> {
    use GrantObjectKind::*;
    match kind {
        DatabaseKind::Postgres => Some(AccessFeatures {
            hosts: false,
            superuser: true,
            create_db: true,
            create_role: true,
            valid_until: true,
            connection_limit: true,
            membership: true,
            grants_per_database: true,
            object_kinds: vec![Database, Schema, Table, AllTables, Sequence, AllSequences],
        }),
        DatabaseKind::Mysql => Some(AccessFeatures {
            hosts: true,
            superuser: false,
            create_db: false,
            create_role: false,
            valid_until: false,
            connection_limit: true,
            membership: true,
            grants_per_database: false,
            object_kinds: vec![Server, Database, Table],
        }),
        DatabaseKind::Sqlite | DatabaseKind::Libsql | DatabaseKind::SqlServer => None,
    }
}

/// The privileges that exist on an object kind, in a sensible order.
pub fn privileges(kind: DatabaseKind, object: GrantObjectKind) -> &'static [&'static str] {
    use GrantObjectKind::*;
    match (kind, object) {
        (DatabaseKind::Postgres, Database) => &["CONNECT", "CREATE", "TEMPORARY"],
        (DatabaseKind::Postgres, Schema) => &["USAGE", "CREATE"],
        (DatabaseKind::Postgres, Table | AllTables) => &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"],
        (DatabaseKind::Postgres, Sequence | AllSequences) => &["USAGE", "SELECT", "UPDATE"],
        (DatabaseKind::Mysql, Server) => &[
            "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "CREATE VIEW",
            "SHOW VIEW", "TRIGGER", "EXECUTE", "CREATE ROUTINE", "ALTER ROUTINE", "EVENT", "LOCK TABLES",
            "CREATE TEMPORARY TABLES", "SHOW DATABASES", "PROCESS", "RELOAD", "CREATE USER", "CREATE ROLE", "DROP ROLE",
            "REPLICATION CLIENT", "REPLICATION SLAVE", "FILE", "SHUTDOWN", "SUPER",
        ],
        (DatabaseKind::Mysql, Database) => &[
            "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "CREATE VIEW",
            "SHOW VIEW", "TRIGGER", "EXECUTE", "CREATE ROUTINE", "ALTER ROUTINE", "EVENT", "LOCK TABLES",
            "CREATE TEMPORARY TABLES",
        ],
        (DatabaseKind::Mysql, Table) => &[
            "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "TRIGGER",
            "CREATE VIEW", "SHOW VIEW",
        ],
        _ => &[],
    }
}

/// The statements for `change`, in order. Fails for invalid input (empty name, unknown privilege…)
/// or something `kind` can't do.
pub fn statements(kind: DatabaseKind, change: &AccessChange) -> Result<Vec<AccessStatement>> {
    let sql = build(kind, change, false)?;
    let display = build(kind, change, true)?;
    Ok(sql.into_iter().zip(display).map(|(sql, display)| AccessStatement { sql, display }).collect())
}

/// The statements for several changes, in order (e.g. create a role, then give it database access),
/// run together in one transaction where the database allows.
pub fn statements_for(kind: DatabaseKind, changes: &[AccessChange]) -> Result<Vec<AccessStatement>> {
    let mut out = Vec::new();
    for change in changes {
        out.extend(statements(kind, change)?);
    }
    Ok(out)
}

/// One generation pass; `mask` replaces passwords with dots (for showing the SQL).
fn build(kind: DatabaseKind, change: &AccessChange, mask: bool) -> Result<Vec<String>> {
    let g = Gen { d: Dialect(kind), mask };
    match kind {
        DatabaseKind::Postgres => match change {
            AccessChange::CreateRole(spec) => g.pg_create(spec),
            AccessChange::AlterRole { role, spec } => g.pg_alter(role, spec),
            AccessChange::DropRole(role) => Ok(vec![format!("DROP ROLE {}", g.role(role)?)]),
            AccessChange::SetPrivileges { role, object, before, after } => g.privileges(role, object, before, after),
            AccessChange::SetDatabaseLevel { role, context, level } => g.pg_level(role, context, *level),
        },
        DatabaseKind::Mysql => match change {
            AccessChange::CreateRole(spec) => g.my_create(spec),
            AccessChange::AlterRole { role, spec } => g.my_alter(role, spec),
            AccessChange::DropRole(role) => Ok(vec![format!("DROP USER {}", g.role(role)?)]),
            AccessChange::SetPrivileges { role, object, before, after } => g.privileges(role, object, before, after),
            AccessChange::SetDatabaseLevel { role, context, level } => {
                check_level(DatabaseKind::Mysql, *level)?;
                if *level == context.level {
                    return Ok(Vec::new());
                }
                let after = PrivilegeSet { privileges: mysql_level_privileges(*level).iter().map(|p| p.to_string()).collect(), grantable: false };
                g.privileges(role, &GrantObject::Database { name: context.database.clone() }, &context.privileges, &after)
            }
        },
        _ => Err(Error::Unsupported(format!("managing users on {} isn’t supported yet", kind.display_name()))),
    }
}

const MASK: &str = "'••••••••'";

struct Gen {
    d: Dialect,
    mask: bool,
}

impl Gen {
    fn kind(&self) -> DatabaseKind {
        self.d.0
    }

    /// `"name"` (Postgres) or `'name'@'host'` (MySQL; host defaults to `%`).
    fn role(&self, role: &RoleRef) -> Result<String> {
        let name = role.name.trim();
        if name.is_empty() {
            return Err(Error::InvalidConfig("Enter a name.".into()));
        }
        Ok(match self.kind() {
            DatabaseKind::Mysql => {
                if name.chars().count() > 32 {
                    return Err(Error::InvalidConfig("MySQL user names are at most 32 characters.".into()));
                }
                let host = role.host.as_deref().map(str::trim).filter(|h| !h.is_empty()).unwrap_or("%");
                format!("{}@{}", self.d.quote_literal(name), self.d.quote_literal(host))
            }
            _ => self.d.quote_ident(name),
        })
    }

    fn spec_ref(spec: &RoleSpec) -> RoleRef {
        RoleRef::new(spec.name.trim(), spec.host.clone())
    }

    fn password(&self, password: &str) -> String {
        if self.mask { MASK.into() } else { self.d.quote_literal(password) }
    }

    fn new_password(spec: &RoleSpec) -> Option<&str> {
        spec.password.as_deref().filter(|p| !p.is_empty())
    }

    fn valid_until(&self, spec: &RoleSpec) -> Option<String> {
        spec.valid_until.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|v| self.d.quote_literal(v))
    }

    // MARK: Postgres

    fn pg_options(&self, spec: &RoleSpec, old: Option<&Role>) -> Vec<String> {
        let mut options = Vec::new();
        let mut flag = |on: bool, was: Option<bool>, yes: &str, no: &str| {
            if was != Some(on) {
                options.push(if on { yes } else { no }.to_string());
            }
        };
        flag(spec.can_login, old.map(|r| r.can_login), "LOGIN", "NOLOGIN");
        flag(spec.is_superuser, old.map(|r| r.is_superuser), "SUPERUSER", "NOSUPERUSER");
        flag(spec.can_create_db, old.map(|r| r.can_create_db), "CREATEDB", "NOCREATEDB");
        flag(spec.can_create_role, old.map(|r| r.can_create_role), "CREATEROLE", "NOCREATEROLE");
        if old.map(|r| r.connection_limit) != Some(spec.connection_limit) && (old.is_some() || spec.connection_limit.is_some()) {
            options.push(format!("CONNECTION LIMIT {}", spec.connection_limit.map_or(-1, i64::from)));
        }
        let valid_until = self.valid_until(spec);
        let old_valid = old.and_then(|r| r.valid_until.as_deref().map(|v| self.d.quote_literal(v)));
        if old.is_some() && valid_until != old_valid || old.is_none() && valid_until.is_some() {
            options.push(format!("VALID UNTIL {}", valid_until.unwrap_or_else(|| "'infinity'".into())));
        }
        if let Some(p) = Self::new_password(spec) {
            options.push(format!("PASSWORD {}", self.password(p)));
        }
        options
    }

    fn pg_create(&self, spec: &RoleSpec) -> Result<Vec<String>> {
        let role = self.role(&Self::spec_ref(spec))?;
        let options = self.pg_options(spec, None);
        let mut out = vec![format!("CREATE ROLE {role} WITH {}", options.join(" "))];
        for parent in &spec.member_of {
            out.push(format!("GRANT {} TO {role}", self.role(parent)?));
        }
        Ok(out)
    }

    fn pg_alter(&self, old: &Role, spec: &RoleSpec) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let old_ref = self.role(&old.reference())?;
        let role = self.role(&Self::spec_ref(spec))?;
        if old_ref != role {
            out.push(format!("ALTER ROLE {old_ref} RENAME TO {role}"));
        }
        let options = self.pg_options(spec, Some(old));
        if !options.is_empty() {
            out.push(format!("ALTER ROLE {role} WITH {}", options.join(" ")));
        }
        out.extend(self.membership(&role, &old.member_of, &spec.member_of)?);
        Ok(out)
    }

    /// `GRANT parent TO role` / `REVOKE parent FROM role` for the difference.
    fn membership(&self, role: &str, before: &[RoleRef], after: &[RoleRef]) -> Result<Vec<String>> {
        let key = |r: &RoleRef| self.role(r);
        let before: BTreeSet<String> = before.iter().map(key).collect::<Result<_>>()?;
        let after: BTreeSet<String> = after.iter().map(key).collect::<Result<_>>()?;
        let mut out: Vec<String> = before.difference(&after).map(|p| format!("REVOKE {p} FROM {role}")).collect();
        out.extend(after.difference(&before).map(|p| format!("GRANT {p} TO {role}")));
        Ok(out)
    }

    // MARK: MySQL

    fn my_create(&self, spec: &RoleSpec) -> Result<Vec<String>> {
        let role = self.role(&Self::spec_ref(spec))?;
        let mut sql = format!("CREATE USER {role}");
        if let Some(p) = Self::new_password(spec) {
            sql += &format!(" IDENTIFIED BY {}", self.password(p));
        }
        if let Some(limit) = spec.connection_limit {
            sql += &format!(" WITH MAX_USER_CONNECTIONS {limit}");
        }
        if !spec.can_login {
            sql += " ACCOUNT LOCK";
        }
        let mut out = vec![sql];
        out.extend(self.membership(&role, &[], &spec.member_of)?);
        if !spec.member_of.is_empty() {
            // Granted roles are inactive until activated; make them active at login.
            out.push(format!("SET DEFAULT ROLE ALL TO {role}"));
        }
        Ok(out)
    }

    fn my_alter(&self, old: &Role, spec: &RoleSpec) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let old_ref = self.role(&old.reference())?;
        let role = self.role(&Self::spec_ref(spec))?;
        if old_ref != role {
            out.push(format!("RENAME USER {old_ref} TO {role}"));
        }
        if let Some(p) = Self::new_password(spec) {
            out.push(format!("ALTER USER {role} IDENTIFIED BY {}", self.password(p)));
        }
        if spec.connection_limit != old.connection_limit {
            out.push(format!("ALTER USER {role} WITH MAX_USER_CONNECTIONS {}", spec.connection_limit.unwrap_or(0)));
        }
        if spec.can_login != old.can_login {
            out.push(format!("ALTER USER {role} ACCOUNT {}", if spec.can_login { "UNLOCK" } else { "LOCK" }));
        }
        let membership = self.membership(&role, &old.member_of, &spec.member_of)?;
        let granted = membership.iter().any(|s| s.starts_with("GRANT"));
        out.extend(membership);
        if granted {
            out.push(format!("SET DEFAULT ROLE ALL TO {role}"));
        }
        Ok(out)
    }

    // MARK: Database levels

    /// Replaces `role`'s privileges in the database (run in that database): revokes what a level may
    /// have granted, then grants the new level on the database, every schema, all tables and
    /// sequences in them, and the same as default privileges for objects created later.
    fn pg_level(&self, role: &RoleRef, ctx: &DatabaseLevelContext, level: DatabaseLevel) -> Result<Vec<String>> {
        check_level(DatabaseKind::Postgres, level)?;
        if level == ctx.level {
            return Ok(Vec::new());
        }
        let r = self.role(role)?;
        let d = self.d;
        let db = d.quote_ident(&ctx.database);
        let mut out = Vec::new();
        if ctx.level != DatabaseLevel::NoAccess {
            out.push(format!("REVOKE ALL ON DATABASE {db} FROM {r}"));
        }
        if !matches!(ctx.level, DatabaseLevel::NoAccess | DatabaseLevel::Connect) {
            for schema in &ctx.schemas {
                let s = d.quote_ident(schema);
                out.push(format!("REVOKE ALL ON SCHEMA {s} FROM {r}"));
                out.push(format!("REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM {r}"));
                out.push(format!("REVOKE ALL ON ALL SEQUENCES IN SCHEMA {s} FROM {r}"));
            }
            for owner in &ctx.owners {
                let o = d.quote_ident(owner);
                for kind in ["TABLES", "SEQUENCES", "SCHEMAS"] {
                    out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {o} REVOKE ALL ON {kind} FROM {r}"));
                }
            }
        }
        let (database, schema, tables, sequences): (&[&str], &[&str], &[&str], &[&str]) = match level {
            DatabaseLevel::NoAccess => return Ok(out),
            DatabaseLevel::Connect => (&["CONNECT"], &[], &[], &[]),
            DatabaseLevel::ReadOnly => (&["CONNECT"], &["USAGE"], PG_TABLE_READ, &["SELECT"]),
            DatabaseLevel::ReadWrite => (&["CONNECT"], &["USAGE"], PG_TABLE_WRITE, &["USAGE", "SELECT", "UPDATE"]),
            DatabaseLevel::SchemaChanges => (&["CONNECT", "CREATE", "TEMPORARY"], &["USAGE", "CREATE"], PG_TABLE_ALL, &["USAGE", "SELECT", "UPDATE"]),
            DatabaseLevel::Custom => unreachable!("checked"),
        };
        out.push(format!("GRANT {} ON DATABASE {db} TO {r}", database.join(", ")));
        if schema.is_empty() {
            return Ok(out);
        }
        for name in &ctx.schemas {
            let s = d.quote_ident(name);
            out.push(format!("GRANT {} ON SCHEMA {s} TO {r}", schema.join(", ")));
            out.push(format!("GRANT {} ON ALL TABLES IN SCHEMA {s} TO {r}", tables.join(", ")));
            out.push(format!("GRANT {} ON ALL SEQUENCES IN SCHEMA {s} TO {r}", sequences.join(", ")));
        }
        for owner in &ctx.owners {
            let o = d.quote_ident(owner);
            out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {o} GRANT {} ON TABLES TO {r}", tables.join(", ")));
            out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {o} GRANT {} ON SEQUENCES TO {r}", sequences.join(", ")));
            out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {o} GRANT {} ON SCHEMAS TO {r}", schema.join(", ")));
        }
        Ok(out)
    }

    // MARK: Privileges

    fn object(&self, object: &GrantObject) -> Result<String> {
        let d = self.d;
        let unsupported = || Error::Unsupported(format!("{} has no such privilege target", self.kind().display_name()));
        Ok(match (self.kind(), object) {
            (DatabaseKind::Postgres, GrantObject::Database { name }) => format!("DATABASE {}", d.quote_ident(name)),
            (DatabaseKind::Postgres, GrantObject::Schema { name }) => format!("SCHEMA {}", d.quote_ident(name)),
            (DatabaseKind::Postgres, GrantObject::Table { schema, name }) => format!("TABLE {}", d.quote_relation(schema, name)),
            (DatabaseKind::Postgres, GrantObject::Sequence { schema, name }) => format!("SEQUENCE {}", d.quote_relation(schema, name)),
            (DatabaseKind::Postgres, GrantObject::AllTables { schema }) => format!("ALL TABLES IN SCHEMA {}", d.quote_ident(schema)),
            (DatabaseKind::Postgres, GrantObject::AllSequences { schema }) => format!("ALL SEQUENCES IN SCHEMA {}", d.quote_ident(schema)),
            (DatabaseKind::Mysql, GrantObject::Server) => "*.*".into(),
            (DatabaseKind::Mysql, GrantObject::Database { name }) => format!("{}.*", d.quote_ident(name)),
            (DatabaseKind::Mysql, GrantObject::Table { schema, name }) => d.quote_relation(schema, name),
            _ => return Err(unsupported()),
        })
    }

    fn privileges(&self, role: &RoleRef, object: &GrantObject, before: &PrivilegeSet, after: &PrivilegeSet) -> Result<Vec<String>> {
        let role = self.role(role)?;
        let on = self.object(object)?;
        let before_set = privilege_set(&before.privileges)?;
        let after_set = privilege_set(&after.privileges)?;
        let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
        let removed: BTreeSet<String> = before_set.difference(&after_set).cloned().collect();
        let added: BTreeSet<String> = after_set.difference(&before_set).cloned().collect();
        let kept: BTreeSet<String> = before_set.intersection(&after_set).cloned().collect();
        let option = if after.grantable { " WITH GRANT OPTION" } else { "" };

        let mut out = Vec::new();
        if !removed.is_empty() {
            out.push(format!("REVOKE {} ON {on} FROM {role}", list(&removed)));
        }
        let loses_option = before.grantable && !before_set.is_empty() && (!after.grantable || after_set.is_empty());
        match self.kind() {
            // MySQL's grant option is per level, not per privilege.
            DatabaseKind::Mysql if loses_option => out.push(format!("REVOKE GRANT OPTION ON {on} FROM {role}")),
            _ if loses_option && !kept.is_empty() => out.push(format!("REVOKE GRANT OPTION FOR {} ON {on} FROM {role}", list(&kept))),
            _ => {}
        }
        if !added.is_empty() {
            out.push(format!("GRANT {} ON {on} TO {role}{option}", list(&added)));
        }
        if after.grantable && !before.grantable && !kept.is_empty() {
            out.push(format!("GRANT {} ON {on} TO {role} WITH GRANT OPTION", list(&kept)));
        }
        Ok(out)
    }
}

/// A level that can be applied on `kind` (`Custom` never can).
fn check_level(kind: DatabaseKind, level: DatabaseLevel) -> Result<()> {
    if database_levels(kind).contains(&level) {
        Ok(())
    } else {
        Err(Error::Unsupported(format!("“{}” isn’t a level on {}", level.title(), kind.display_name())))
    }
}

/// Privilege names, upper-cased and checked to be keywords (they go into the SQL unquoted).
fn privilege_set(privileges: &[String]) -> Result<BTreeSet<String>> {
    privileges
        .iter()
        .map(|p| {
            let p = p.trim().to_ascii_uppercase();
            let ok = p.starts_with(|c: char| c.is_ascii_uppercase()) && p.chars().all(|c| c.is_ascii_uppercase() || c == ' ' || c == '_');
            if ok { Ok(p) } else { Err(Error::Query(format!("“{p}” isn’t a privilege"))) }
        })
        .collect()
}

/// Groups grants by object (sorted), merging duplicates from different grantors.
pub fn group_grants(grants: &[Grant]) -> Vec<(GrantObject, PrivilegeSet)> {
    let mut objects: Vec<(GrantObject, PrivilegeSet)> = Vec::new();
    let mut sorted: Vec<&Grant> = grants.iter().collect();
    sorted.sort();
    for g in sorted {
        if objects.last().is_none_or(|(o, _)| *o != g.object) {
            objects.push((g.object.clone(), PrivilegeSet { privileges: Vec::new(), grantable: true }));
        }
        let (_, set) = objects.last_mut().unwrap();
        if !set.privileges.contains(&g.privilege) {
            set.privileges.push(g.privilege.clone());
        }
        set.grantable &= g.grantable;
    }
    objects
}

/// Catalog row → [`GrantObject`]: `kind` is `server`, `database`, `schema`, `table` or `sequence`.
pub(crate) fn object_from_catalog(kind: &str, schema: Option<String>, name: Option<String>) -> Option<GrantObject> {
    Some(match kind {
        "server" => GrantObject::Server,
        "database" => GrantObject::Database { name: name? },
        "schema" => GrantObject::Schema { name: name? },
        "table" => GrantObject::Table { schema: schema?, name: name? },
        "sequence" => GrantObject::Sequence { schema: schema?, name: name? },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(kind: DatabaseKind, change: AccessChange) -> Vec<String> {
        statements(kind, &change).unwrap().into_iter().map(|s| s.sql).collect()
    }

    fn display(kind: DatabaseKind, change: AccessChange) -> Vec<String> {
        statements(kind, &change).unwrap().into_iter().map(|s| s.display).collect()
    }

    fn spec(name: &str) -> RoleSpec {
        RoleSpec { name: name.into(), can_login: true, ..Default::default() }
    }

    fn role(name: &str) -> Role {
        Role {
            name: name.into(),
            host: None,
            can_login: true,
            is_superuser: false,
            can_create_db: false,
            can_create_role: false,
            is_system: false,
            connection_limit: None,
            valid_until: None,
            member_of: vec![],
            comment: None,
        }
    }

    fn set(privileges: &[&str], grantable: bool) -> PrivilegeSet {
        PrivilegeSet { privileges: privileges.iter().map(|s| s.to_string()).collect(), grantable }
    }

    #[test]
    fn creates_a_postgres_login_role_with_memberships() {
        let mut s = spec("app");
        s.password = Some("it's secret".into());
        s.connection_limit = Some(5);
        s.valid_until = Some("2030-01-01".into());
        s.member_of = vec![RoleRef::new("readers", None)];
        assert_eq!(
            sql(DatabaseKind::Postgres, AccessChange::CreateRole(s.clone())),
            [
                r#"CREATE ROLE "app" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE CONNECTION LIMIT 5 VALID UNTIL '2030-01-01' PASSWORD 'it''s secret'"#,
                r#"GRANT "readers" TO "app""#,
            ]
        );
        let shown = display(DatabaseKind::Postgres, AccessChange::CreateRole(s));
        assert!(shown[0].ends_with("PASSWORD '••••••••'"), "{}", shown[0]);
        assert!(!shown.join(" ").contains("secret"));
    }

    #[test]
    fn alters_only_what_changed() {
        let old = role("app");
        let mut s = spec("app2");
        s.is_superuser = true;
        s.member_of = vec![RoleRef::new("writers", None)];
        let mut before = old.clone();
        before.member_of = vec![RoleRef::new("readers", None)];
        assert_eq!(
            sql(DatabaseKind::Postgres, AccessChange::AlterRole { role: before, spec: s }),
            [
                r#"ALTER ROLE "app" RENAME TO "app2""#,
                r#"ALTER ROLE "app2" WITH SUPERUSER"#,
                r#"REVOKE "readers" FROM "app2""#,
                r#"GRANT "writers" TO "app2""#,
            ]
        );
        // Nothing changed, no password: nothing to run.
        assert!(sql(DatabaseKind::Postgres, AccessChange::AlterRole { role: old, spec: spec("app") }).is_empty());
    }

    #[test]
    fn clears_postgres_limits() {
        let mut old = role("app");
        old.connection_limit = Some(3);
        old.valid_until = Some("2030-01-01 00:00:00+00".into());
        assert_eq!(
            sql(DatabaseKind::Postgres, AccessChange::AlterRole { role: old, spec: spec("app") }),
            [r#"ALTER ROLE "app" WITH CONNECTION LIMIT -1 VALID UNTIL 'infinity'"#]
        );
    }

    #[test]
    fn creates_and_alters_mysql_accounts() {
        let mut s = spec("app");
        s.host = Some("10.0.%".into());
        s.password = Some("p\\w".into());
        s.member_of = vec![RoleRef::new("reader", Some("%".into()))];
        assert_eq!(
            sql(DatabaseKind::Mysql, AccessChange::CreateRole(s)),
            [
                r"CREATE USER 'app'@'10.0.%' IDENTIFIED BY 'p\\w'",
                "GRANT 'reader'@'%' TO 'app'@'10.0.%'",
                "SET DEFAULT ROLE ALL TO 'app'@'10.0.%'",
            ]
        );
        let mut group = spec("reader");
        group.can_login = false;
        assert_eq!(sql(DatabaseKind::Mysql, AccessChange::CreateRole(group)), ["CREATE USER 'reader'@'%' ACCOUNT LOCK"]);

        let mut old = role("app");
        old.host = Some("%".into());
        let mut s = spec("app");
        s.host = Some("localhost".into());
        s.can_login = false;
        s.connection_limit = Some(10);
        assert_eq!(
            sql(DatabaseKind::Mysql, AccessChange::AlterRole { role: old, spec: s }),
            [
                "RENAME USER 'app'@'%' TO 'app'@'localhost'",
                "ALTER USER 'app'@'localhost' WITH MAX_USER_CONNECTIONS 10",
                "ALTER USER 'app'@'localhost' ACCOUNT LOCK",
            ]
        );
        assert_eq!(sql(DatabaseKind::Mysql, AccessChange::DropRole(RoleRef::new("app", None))), ["DROP USER 'app'@'%'"]);
    }

    #[test]
    fn diffs_postgres_privileges() {
        let r = RoleRef::new("app", None);
        let table = GrantObject::Table { schema: "public".into(), name: "orders".into() };
        assert_eq!(
            sql(
                DatabaseKind::Postgres,
                AccessChange::SetPrivileges {
                    role: r.clone(),
                    object: table.clone(),
                    before: set(&["SELECT", "DELETE"], false),
                    after: set(&["SELECT", "insert", "UPDATE"], false),
                }
            ),
            [
                r#"REVOKE DELETE ON TABLE "public"."orders" FROM "app""#,
                r#"GRANT INSERT, UPDATE ON TABLE "public"."orders" TO "app""#,
            ]
        );
        // Dropping the grant option keeps the privileges.
        assert_eq!(
            sql(
                DatabaseKind::Postgres,
                AccessChange::SetPrivileges { role: r.clone(), object: table.clone(), before: set(&["SELECT"], true), after: set(&["SELECT"], false) }
            ),
            [r#"REVOKE GRANT OPTION FOR SELECT ON TABLE "public"."orders" FROM "app""#]
        );
        assert_eq!(
            sql(
                DatabaseKind::Postgres,
                AccessChange::SetPrivileges {
                    role: r,
                    object: GrantObject::AllTables { schema: "billing".into() },
                    before: set(&[], false),
                    after: set(&["SELECT"], true),
                }
            ),
            [r#"GRANT SELECT ON ALL TABLES IN SCHEMA "billing" TO "app" WITH GRANT OPTION"#]
        );
    }

    #[test]
    fn diffs_mysql_privileges() {
        let r = RoleRef::new("app", Some("%".into()));
        assert_eq!(
            sql(
                DatabaseKind::Mysql,
                AccessChange::SetPrivileges {
                    role: r.clone(),
                    object: GrantObject::Database { name: "shop".into() },
                    before: set(&["SELECT", "INSERT"], true),
                    after: set(&["SELECT", "CREATE VIEW"], false),
                }
            ),
            [
                "REVOKE INSERT ON `shop`.* FROM 'app'@'%'",
                "REVOKE GRANT OPTION ON `shop`.* FROM 'app'@'%'",
                "GRANT CREATE VIEW ON `shop`.* TO 'app'@'%'",
            ]
        );
        assert_eq!(
            sql(
                DatabaseKind::Mysql,
                AccessChange::SetPrivileges { role: r, object: GrantObject::Server, before: set(&[], false), after: set(&["PROCESS"], false) }
            ),
            ["GRANT PROCESS ON *.* TO 'app'@'%'"]
        );
    }

    #[test]
    fn rejects_bad_input() {
        let r = RoleRef::new("app", None);
        let bad = AccessChange::SetPrivileges {
            role: r.clone(),
            object: GrantObject::Schema { name: "public".into() },
            before: set(&[], false),
            after: set(&["SELECT; DROP TABLE x"], false),
        };
        assert!(statements(DatabaseKind::Postgres, &bad).is_err());
        assert!(statements(DatabaseKind::Postgres, &AccessChange::CreateRole(spec("  "))).is_err());
        // Postgres has no server-wide privileges, MySQL no schemas apart from databases.
        let server = AccessChange::SetPrivileges { role: r, object: GrantObject::Server, before: set(&[], false), after: set(&["SELECT"], false) };
        assert!(statements(DatabaseKind::Postgres, &server).is_err());
        assert!(statements(DatabaseKind::Sqlite, &server).is_err());
        assert!(features(DatabaseKind::Sqlite).is_none());
    }

    #[test]
    fn generates_strong_passwords() {
        let a = generate_password(24).unwrap();
        let b = generate_password(24).unwrap();
        assert_eq!(a.len(), 24);
        assert_ne!(a, b);
        assert!(a.chars().any(|c| c.is_ascii_uppercase()) && a.chars().any(|c| c.is_ascii_lowercase()));
        assert!(a.chars().any(|c| c.is_ascii_digit()) && a.chars().any(|c| "-_.~".contains(c)));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c)));
        assert_eq!(generate_password(4).unwrap().len(), 12);
    }

    #[test]
    fn batches_changes_in_order() {
        let changes = [
            AccessChange::CreateRole(spec("app")),
            AccessChange::SetPrivileges {
                role: RoleRef::new("app", None),
                object: GrantObject::Database { name: "other_db".into() },
                before: set(&[], false),
                after: set(&["CONNECT"], false),
            },
        ];
        let sql: Vec<String> = statements_for(DatabaseKind::Postgres, &changes).unwrap().into_iter().map(|s| s.sql).collect();
        assert_eq!(sql.len(), 2);
        assert_eq!(sql[1], r#"GRANT CONNECT ON DATABASE "other_db" TO "app""#);
    }

    fn context(level: DatabaseLevel) -> DatabaseLevelContext {
        DatabaseLevelContext {
            database: "shop".into(),
            level,
            privileges: PrivilegeSet::default(),
            schemas: vec!["public".into(), "billing".into()],
            owners: vec!["app_owner".into()],
        }
    }

    fn level_sql(kind: DatabaseKind, ctx: DatabaseLevelContext, level: DatabaseLevel) -> Vec<String> {
        sql(kind, AccessChange::SetDatabaseLevel { role: RoleRef::new("reader", None), context: ctx, level })
    }

    #[test]
    fn grants_a_postgres_read_only_level_with_default_privileges() {
        let sql = level_sql(DatabaseKind::Postgres, context(DatabaseLevel::NoAccess), DatabaseLevel::ReadOnly);
        assert_eq!(
            sql,
            [
                r#"GRANT CONNECT ON DATABASE "shop" TO "reader""#,
                r#"GRANT USAGE ON SCHEMA "public" TO "reader""#,
                r#"GRANT SELECT ON ALL TABLES IN SCHEMA "public" TO "reader""#,
                r#"GRANT SELECT ON ALL SEQUENCES IN SCHEMA "public" TO "reader""#,
                r#"GRANT USAGE ON SCHEMA "billing" TO "reader""#,
                r#"GRANT SELECT ON ALL TABLES IN SCHEMA "billing" TO "reader""#,
                r#"GRANT SELECT ON ALL SEQUENCES IN SCHEMA "billing" TO "reader""#,
                r#"ALTER DEFAULT PRIVILEGES FOR ROLE "app_owner" GRANT SELECT ON TABLES TO "reader""#,
                r#"ALTER DEFAULT PRIVILEGES FOR ROLE "app_owner" GRANT SELECT ON SEQUENCES TO "reader""#,
                r#"ALTER DEFAULT PRIVILEGES FOR ROLE "app_owner" GRANT USAGE ON SCHEMAS TO "reader""#,
            ]
        );
        // Changing level first revokes everything a level grants; the same level is a no-op.
        let down = level_sql(DatabaseKind::Postgres, context(DatabaseLevel::ReadWrite), DatabaseLevel::Connect);
        assert_eq!(down[0], r#"REVOKE ALL ON DATABASE "shop" FROM "reader""#);
        assert!(down.contains(&r#"ALTER DEFAULT PRIVILEGES FOR ROLE "app_owner" REVOKE ALL ON TABLES FROM "reader""#.to_string()));
        assert_eq!(down.last().unwrap(), r#"GRANT CONNECT ON DATABASE "shop" TO "reader""#);
        assert!(level_sql(DatabaseKind::Postgres, context(DatabaseLevel::ReadOnly), DatabaseLevel::ReadOnly).is_empty());
        let none = level_sql(DatabaseKind::Postgres, context(DatabaseLevel::Connect), DatabaseLevel::NoAccess);
        assert_eq!(none, [r#"REVOKE ALL ON DATABASE "shop" FROM "reader""#]);
        let bad = AccessChange::SetDatabaseLevel { role: RoleRef::new("r", None), context: context(DatabaseLevel::NoAccess), level: DatabaseLevel::Custom };
        assert!(statements(DatabaseKind::Postgres, &bad).is_err());
        assert_eq!(bad.database(DatabaseKind::Postgres), Some("shop"));
        assert_eq!(bad.database(DatabaseKind::Mysql), None);
    }

    #[test]
    fn sets_mysql_levels_as_database_privileges() {
        let mut ctx = context(DatabaseLevel::ReadOnly);
        ctx.privileges = set(&["SELECT", "SHOW VIEW"], false);
        let sql = level_sql(DatabaseKind::Mysql, ctx, DatabaseLevel::ReadWrite);
        assert_eq!(sql, ["GRANT CREATE TEMPORARY TABLES, DELETE, EXECUTE, INSERT, LOCK TABLES, UPDATE ON `shop`.* TO 'reader'@'%'"]);
        assert_eq!(mysql_level(&["SHOW VIEW".into(), "SELECT".into()]), DatabaseLevel::ReadOnly);
        assert_eq!(mysql_level(&["SELECT".into()]), DatabaseLevel::Custom);
        assert_eq!(mysql_level(&[]), DatabaseLevel::NoAccess);
        assert!(!database_levels(DatabaseKind::Mysql).contains(&DatabaseLevel::Connect));
    }

    #[test]
    fn classifies_postgres_levels() {
        let base = PgLevelFacts { schemas: 2, relations: 3, writable: 2, ..Default::default() };
        assert_eq!(pg_level(&base), DatabaseLevel::NoAccess);
        let connect = PgLevelFacts { database: vec!["CONNECT".into()], ..base.clone() };
        assert_eq!(pg_level(&connect), DatabaseLevel::Connect);
        let read = PgLevelFacts { schemas_usage: 2, relations_select: 3, default_table: vec!["SELECT".into()], ..connect.clone() };
        assert_eq!(pg_level(&read), DatabaseLevel::ReadOnly);
        let write = PgLevelFacts { writable_write: 2, any_write: 2, default_table: vec!["SELECT".into(), "INSERT".into(), "UPDATE".into(), "DELETE".into()], ..read.clone() };
        assert_eq!(pg_level(&write), DatabaseLevel::ReadWrite);
        let all = PgLevelFacts {
            database: vec!["CONNECT".into(), "CREATE".into(), "TEMPORARY".into()],
            schemas_create: 2,
            writable_ddl: 2,
            any_ddl: 2,
            ..write.clone()
        };
        assert_eq!(pg_level(&all), DatabaseLevel::SchemaChanges);
        // One table missing SELECT: not a level.
        assert_eq!(pg_level(&PgLevelFacts { relations_select: 2, ..read.clone() }), DatabaseLevel::Custom);
        // No tables yet: the default privileges decide.
        let empty = PgLevelFacts { relations: 0, writable: 0, relations_select: 0, ..read };
        assert_eq!(pg_level(&empty), DatabaseLevel::ReadOnly);
    }

    #[test]
    fn groups_grants_by_object() {
        let t = GrantObject::Table { schema: "public".into(), name: "a".into() };
        let grants = vec![
            Grant { object: t.clone(), privilege: "SELECT".into(), grantable: true },
            Grant { object: GrantObject::Schema { name: "public".into() }, privilege: "USAGE".into(), grantable: false },
            Grant { object: t.clone(), privilege: "INSERT".into(), grantable: false },
            Grant { object: t.clone(), privilege: "SELECT".into(), grantable: true },
        ];
        let grouped = group_grants(&grants);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[1], (t, set(&["INSERT", "SELECT"], false)));
    }
}
