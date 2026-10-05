//! Users, roles and privileges (`dbcore::access`).

use dbcore::access as core;

use crate::{Connection, DatabaseKind, DbError};

#[derive(uniffi::Record, Clone)]
pub struct RoleRef {
    pub name: String,
    pub host: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct Role {
    pub name: String,
    pub host: Option<String>,
    pub can_login: bool,
    pub is_superuser: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    pub is_system: bool,
    pub connection_limit: Option<u32>,
    pub valid_until: Option<String>,
    pub member_of: Vec<RoleRef>,
    pub comment: Option<String>,
}

#[derive(uniffi::Record, Clone)]
pub struct RoleSpec {
    pub name: String,
    pub host: Option<String>,
    pub password: Option<String>,
    pub can_login: bool,
    pub is_superuser: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
    pub connection_limit: Option<u32>,
    pub valid_until: Option<String>,
    pub member_of: Vec<RoleRef>,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum GrantObjectKind {
    Server,
    Database,
    Schema,
    Table,
    Sequence,
    AllTables,
    AllSequences,
}

#[derive(uniffi::Enum, Clone)]
pub enum GrantObject {
    Server,
    Database { name: String },
    Schema { name: String },
    Table { schema: String, name: String },
    Sequence { schema: String, name: String },
    AllTables { schema: String },
    AllSequences { schema: String },
}

#[derive(uniffi::Record, Clone)]
pub struct PrivilegeSet {
    pub privileges: Vec<String>,
    pub grantable: bool,
}

/// The privileges a role holds on one object.
#[derive(uniffi::Record, Clone)]
pub struct ObjectPrivileges {
    pub object: GrantObject,
    pub privileges: PrivilegeSet,
}

#[derive(uniffi::Enum, Clone)]
pub enum AccessChange {
    CreateRole { spec: RoleSpec },
    AlterRole { role: Role, spec: RoleSpec },
    DropRole { role: RoleRef },
    SetPrivileges { role: RoleRef, object: GrantObject, before: PrivilegeSet, after: PrivilegeSet },
}

#[derive(uniffi::Record)]
pub struct AccessStatement {
    pub sql: String,
    /// `sql` with passwords masked, for showing.
    pub display: String,
}

#[derive(uniffi::Record)]
pub struct AccessFeatures {
    pub hosts: bool,
    pub superuser: bool,
    pub create_db: bool,
    pub create_role: bool,
    pub valid_until: bool,
    pub connection_limit: bool,
    pub membership: bool,
    pub grants_per_database: bool,
    pub object_kinds: Vec<GrantObjectKind>,
}

/// What can be managed on `kind`; `None` if users can't be managed there (yet).
#[uniffi::export]
pub fn access_features(kind: DatabaseKind) -> Option<AccessFeatures> {
    core::features(kind.into()).map(|f| AccessFeatures {
        hosts: f.hosts,
        superuser: f.superuser,
        create_db: f.create_db,
        create_role: f.create_role,
        valid_until: f.valid_until,
        connection_limit: f.connection_limit,
        membership: f.membership,
        grants_per_database: f.grants_per_database,
        object_kinds: f.object_kinds.into_iter().map(Into::into).collect(),
    })
}

/// The privileges that exist on an object kind, in display order.
#[uniffi::export]
pub fn access_privileges(kind: DatabaseKind, object: GrantObjectKind) -> Vec<String> {
    core::privileges(kind.into(), object.into()).iter().map(|s| s.to_string()).collect()
}

/// Grants grouped by object (sorted), one entry per object.
#[uniffi::export]
pub fn group_grants(grants: Vec<Grant>) -> Vec<ObjectPrivileges> {
    let grants: Vec<core::Grant> = grants.into_iter().map(Into::into).collect();
    core::group_grants(&grants).into_iter().map(|(object, privileges)| ObjectPrivileges { object: object.into(), privileges: privileges.into() }).collect()
}

#[derive(uniffi::Record, Clone)]
pub struct Grant {
    pub object: GrantObject,
    pub privilege: String,
    pub grantable: bool,
}

#[uniffi::export]
impl Connection {
    /// Users and roles on the server, system ones included.
    pub async fn list_roles(&self) -> Result<Vec<Role>, DbError> {
        Ok(self.inner.list_roles().await?.into_iter().map(Into::into).collect())
    }

    /// Privileges granted directly to `role` (Postgres: in this connection's database).
    pub async fn list_grants(&self, role: RoleRef) -> Result<Vec<Grant>, DbError> {
        Ok(self.inner.list_grants(role.into()).await?.into_iter().map(Into::into).collect())
    }

    /// The statements `apply_access` would run.
    pub fn preview_access(&self, change: AccessChange) -> Result<Vec<AccessStatement>, DbError> {
        Ok(self
            .inner
            .preview_access(&change.into())?
            .into_iter()
            .map(|s| AccessStatement { sql: s.sql, display: s.display })
            .collect())
    }

    /// Creates, changes or drops a role, or changes its privileges.
    pub async fn apply_access(&self, change: AccessChange) -> Result<(), DbError> {
        Ok(self.inner.apply_access(change.into()).await?)
    }
}

// MARK: Conversions

impl From<RoleRef> for core::RoleRef {
    fn from(r: RoleRef) -> Self {
        Self { name: r.name, host: r.host }
    }
}

impl From<core::RoleRef> for RoleRef {
    fn from(r: core::RoleRef) -> Self {
        Self { name: r.name, host: r.host }
    }
}

impl From<core::Role> for Role {
    fn from(r: core::Role) -> Self {
        Self {
            name: r.name,
            host: r.host,
            can_login: r.can_login,
            is_superuser: r.is_superuser,
            can_create_db: r.can_create_db,
            can_create_role: r.can_create_role,
            is_system: r.is_system,
            connection_limit: r.connection_limit,
            valid_until: r.valid_until,
            member_of: r.member_of.into_iter().map(Into::into).collect(),
            comment: r.comment,
        }
    }
}

impl From<Role> for core::Role {
    fn from(r: Role) -> Self {
        Self {
            name: r.name,
            host: r.host,
            can_login: r.can_login,
            is_superuser: r.is_superuser,
            can_create_db: r.can_create_db,
            can_create_role: r.can_create_role,
            is_system: r.is_system,
            connection_limit: r.connection_limit,
            valid_until: r.valid_until,
            member_of: r.member_of.into_iter().map(Into::into).collect(),
            comment: r.comment,
        }
    }
}

impl From<RoleSpec> for core::RoleSpec {
    fn from(s: RoleSpec) -> Self {
        Self {
            name: s.name,
            host: s.host,
            password: s.password,
            can_login: s.can_login,
            is_superuser: s.is_superuser,
            can_create_db: s.can_create_db,
            can_create_role: s.can_create_role,
            connection_limit: s.connection_limit,
            valid_until: s.valid_until,
            member_of: s.member_of.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<GrantObjectKind> for core::GrantObjectKind {
    fn from(k: GrantObjectKind) -> Self {
        match k {
            GrantObjectKind::Server => Self::Server,
            GrantObjectKind::Database => Self::Database,
            GrantObjectKind::Schema => Self::Schema,
            GrantObjectKind::Table => Self::Table,
            GrantObjectKind::Sequence => Self::Sequence,
            GrantObjectKind::AllTables => Self::AllTables,
            GrantObjectKind::AllSequences => Self::AllSequences,
        }
    }
}

impl From<core::GrantObjectKind> for GrantObjectKind {
    fn from(k: core::GrantObjectKind) -> Self {
        match k {
            core::GrantObjectKind::Server => Self::Server,
            core::GrantObjectKind::Database => Self::Database,
            core::GrantObjectKind::Schema => Self::Schema,
            core::GrantObjectKind::Table => Self::Table,
            core::GrantObjectKind::Sequence => Self::Sequence,
            core::GrantObjectKind::AllTables => Self::AllTables,
            core::GrantObjectKind::AllSequences => Self::AllSequences,
        }
    }
}

impl From<GrantObject> for core::GrantObject {
    fn from(o: GrantObject) -> Self {
        match o {
            GrantObject::Server => Self::Server,
            GrantObject::Database { name } => Self::Database { name },
            GrantObject::Schema { name } => Self::Schema { name },
            GrantObject::Table { schema, name } => Self::Table { schema, name },
            GrantObject::Sequence { schema, name } => Self::Sequence { schema, name },
            GrantObject::AllTables { schema } => Self::AllTables { schema },
            GrantObject::AllSequences { schema } => Self::AllSequences { schema },
        }
    }
}

impl From<core::GrantObject> for GrantObject {
    fn from(o: core::GrantObject) -> Self {
        match o {
            core::GrantObject::Server => Self::Server,
            core::GrantObject::Database { name } => Self::Database { name },
            core::GrantObject::Schema { name } => Self::Schema { name },
            core::GrantObject::Table { schema, name } => Self::Table { schema, name },
            core::GrantObject::Sequence { schema, name } => Self::Sequence { schema, name },
            core::GrantObject::AllTables { schema } => Self::AllTables { schema },
            core::GrantObject::AllSequences { schema } => Self::AllSequences { schema },
        }
    }
}

impl From<PrivilegeSet> for core::PrivilegeSet {
    fn from(s: PrivilegeSet) -> Self {
        Self { privileges: s.privileges, grantable: s.grantable }
    }
}

impl From<core::PrivilegeSet> for PrivilegeSet {
    fn from(s: core::PrivilegeSet) -> Self {
        Self { privileges: s.privileges, grantable: s.grantable }
    }
}

impl From<core::Grant> for Grant {
    fn from(g: core::Grant) -> Self {
        Self { object: g.object.into(), privilege: g.privilege, grantable: g.grantable }
    }
}

impl From<Grant> for core::Grant {
    fn from(g: Grant) -> Self {
        Self { object: g.object.into(), privilege: g.privilege, grantable: g.grantable }
    }
}

impl From<AccessChange> for core::AccessChange {
    fn from(c: AccessChange) -> Self {
        match c {
            AccessChange::CreateRole { spec } => Self::CreateRole(spec.into()),
            AccessChange::AlterRole { role, spec } => Self::AlterRole { role: role.into(), spec: spec.into() },
            AccessChange::DropRole { role } => Self::DropRole(role.into()),
            AccessChange::SetPrivileges { role, object, before, after } => {
                Self::SetPrivileges { role: role.into(), object: object.into(), before: before.into(), after: after.into() }
            }
        }
    }
}
