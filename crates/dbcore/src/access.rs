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

/// One generation pass; `mask` replaces passwords with dots (for showing the SQL).
fn build(kind: DatabaseKind, change: &AccessChange, mask: bool) -> Result<Vec<String>> {
    let g = Gen { d: Dialect(kind), mask };
    match kind {
        DatabaseKind::Postgres => match change {
            AccessChange::CreateRole(spec) => g.pg_create(spec),
            AccessChange::AlterRole { role, spec } => g.pg_alter(role, spec),
            AccessChange::DropRole(role) => Ok(vec![format!("DROP ROLE {}", g.role(role)?)]),
            AccessChange::SetPrivileges { role, object, before, after } => g.privileges(role, object, before, after),
        },
        DatabaseKind::Mysql => match change {
            AccessChange::CreateRole(spec) => g.my_create(spec),
            AccessChange::AlterRole { role, spec } => g.my_alter(role, spec),
            AccessChange::DropRole(role) => Ok(vec![format!("DROP USER {}", g.role(role)?)]),
            AccessChange::SetPrivileges { role, object, before, after } => g.privileges(role, object, before, after),
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
