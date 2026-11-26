//! Editing a table's rows from a client that sends its changes as data ([`RowChange`]): what can be
//! edited and why not ([`TableEditing`]), the checks the changes must pass, and their SQL. The CLI's
//! `table.editable` / `table.changes.preview` / `table.changes.apply` are built on it, so editors
//! (the Neovim browser) never build SQL themselves.
//!
//! The rules are the table grids': a table is editable when it isn't a view and has a primary key;
//! binary cells reach clients as a hex preview, so they're never written from text (NULL and
//! DEFAULT are fine); UPDATEs and DELETEs name the row by its whole primary key as loaded.

use std::sync::Arc;

use dbcore::edit::{self, EditStatement, EditValue, KeyValue, RowChange};
use dbcore::results::ResultSources;
use dbcore::{ColumnInfo, Connection, Error, Result, TableInfo, TableStructure};
use serde::{Deserialize, Serialize};

use crate::{Service, Target};

/// A column of a table being edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditableColumn {
    pub name: String,
    pub type_name: String,
    pub is_primary_key: bool,
    pub is_nullable: bool,
    /// As the database spells it (`nextval(…)`, `CURRENT_TIMESTAMP`…).
    pub default_value: Option<String>,
    /// Shown as a hex preview (`0x…`, cut): only NULL or DEFAULT can be written.
    pub binary: bool,
    /// Why its cells can't be changed (`None`: they can).
    pub read_only: Option<String>,
}

/// A foreign key of the table: a row's `columns` point at `target_columns` of `schema.table`
/// (empty: its primary key, an implicit SQLite reference).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyRef {
    pub columns: Vec<String>,
    pub schema: String,
    pub table: String,
    pub target_columns: Vec<String>,
    pub label: String,
}

/// Another table's foreign key pointing here: its `columns` hold this table's `values` columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceRef {
    pub values: Vec<String>,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub label: String,
}

/// What can be edited in a table, by column name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableEditing {
    pub table: TableInfo,
    /// Why its rows can't be edited at all (a view, no primary key). `None`: they can.
    pub read_only: Option<String>,
    /// Primary key columns in key order.
    pub key: Vec<String>,
    pub columns: Vec<EditableColumn>,
    pub foreign_keys: Vec<ForeignKeyRef>,
    pub referenced_by: Vec<ReferenceRef>,
}

impl TableEditing {
    pub fn new(table: &TableInfo, structure: &TableStructure) -> Self {
        let infos: Vec<ColumnInfo> = structure
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                type_name: c.type_name.clone(),
                is_primary_key: c.is_primary_key || structure.primary_key.contains(&c.name),
                is_nullable: c.is_nullable,
            })
            .collect();
        let sources = ResultSources::for_table(table, &infos, structure);
        let read_only = match sources.tables.first() {
            Some(t) => t.read_only.clone(),
            None => Some(format!("“{}” has no columns.", table.name)),
        };
        let name = |i: usize| infos[i].name.clone();
        let columns = structure
            .columns
            .iter()
            .zip(&infos)
            .map(|(detail, info)| {
                let binary = edit::is_binary(info);
                EditableColumn {
                    name: info.name.clone(),
                    type_name: info.type_name.clone(),
                    is_primary_key: info.is_primary_key,
                    is_nullable: info.is_nullable,
                    default_value: detail.default_value.clone(),
                    binary,
                    read_only: read_only.clone().or_else(|| binary.then(|| "Binary values can’t be edited as text.".to_string())),
                }
            })
            .collect();
        Self {
            table: table.clone(),
            key: if read_only.is_none() { structure.primary_key.clone() } else { Vec::new() },
            read_only,
            columns,
            foreign_keys: sources
                .foreign_keys
                .iter()
                .map(|l| ForeignKeyRef {
                    columns: l.columns.iter().map(|&i| name(i)).collect(),
                    schema: l.schema.clone(),
                    table: l.table.clone(),
                    target_columns: l.target_columns.clone(),
                    label: l.label.clone(),
                })
                .collect(),
            referenced_by: sources
                .referenced_by
                .iter()
                .map(|r| ReferenceRef {
                    values: r.values.iter().map(|&i| name(i)).collect(),
                    schema: r.schema.clone(),
                    table: r.table.clone(),
                    columns: r.columns.clone(),
                    label: r.label.clone(),
                })
                .collect(),
        }
    }

    /// The columns as [`edit::statements`] takes them (primary key flagged).
    pub fn column_infos(&self) -> Vec<ColumnInfo> {
        self.columns
            .iter()
            .map(|c| ColumnInfo { name: c.name.clone(), type_name: c.type_name.clone(), is_primary_key: c.is_primary_key, is_nullable: c.is_nullable })
            .collect()
    }

    fn column(&self, name: &str) -> Result<&EditableColumn> {
        self.columns.iter().find(|c| c.name == name).ok_or_else(|| Error::Query(format!("“{}” has no column “{name}”.", self.table.name)))
    }

    /// Refuses changes the table can't take: any change to a read-only table, text into a binary
    /// or read-only column, a row named by anything but its whole primary key.
    pub fn check(&self, changes: &[RowChange]) -> Result<()> {
        if let Some(reason) = &self.read_only {
            if !changes.is_empty() {
                return Err(Error::Unsupported(reason.clone()));
            }
        }
        let check_key = |key: &[KeyValue]| -> Result<()> {
            let mut names: Vec<&str> = key.iter().map(|k| k.column.as_str()).collect();
            names.sort_unstable();
            let mut want: Vec<&str> = self.key.iter().map(String::as_str).collect();
            want.sort_unstable();
            if names != want {
                return Err(Error::Query(format!("A row of “{}” must be named by its whole primary key ({}).", self.table.name, self.key.join(", "))));
            }
            Ok(())
        };
        let check_value = |column: &str, value: &EditValue| -> Result<()> {
            let c = self.column(column)?;
            match (&c.read_only, value) {
                (Some(_), EditValue::Null | EditValue::Default) if c.binary => Ok(()),
                (Some(reason), _) => Err(Error::Unsupported(format!("“{column}”: {reason}"))),
                (None, _) => Ok(()),
            }
        };
        for change in changes {
            match change {
                RowChange::Delete { key } => check_key(key)?,
                RowChange::Update { key, set } => {
                    check_key(key)?;
                    for e in set {
                        check_value(&e.column, &e.value)?;
                    }
                }
                RowChange::Insert { values } => {
                    for e in values {
                        check_value(&e.column, &e.value)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Service {
    /// What can be edited in `table` (described on the target's shared connection).
    pub async fn table_editing(&self, target: &Target, table: &TableInfo) -> Result<(TableEditing, TableStructure)> {
        let structure = self.connection(target).await?.describe_table(table.clone()).await?;
        Ok((TableEditing::new(table, &structure), structure))
    }

    /// The statements that [`apply_table_changes`](Self::apply_table_changes) would run, after
    /// [`TableEditing::check`]. Deletes first, then updates, then inserts.
    pub async fn preview_table_changes(&self, target: &Target, table: &TableInfo, changes: &[RowChange]) -> Result<Vec<EditStatement>> {
        Ok(self.checked(target, table, changes).await?.1)
    }

    /// Saves `changes` in one transaction (all or nothing; each UPDATE and DELETE must match
    /// exactly one row). With `reviewed` (the SQL the user confirmed), refuses to run anything
    /// else, e.g. when the table changed since the preview. Returns the rows affected.
    pub async fn apply_table_changes(&self, target: &Target, table: &TableInfo, changes: Vec<RowChange>, reviewed: Option<&[String]>) -> Result<u64> {
        let (editing, statements, connection) = self.checked(target, table, &changes).await?;
        if let Some(reviewed) = reviewed {
            if !statements.iter().map(|s| s.sql.as_str()).eq(reviewed.iter().map(String::as_str)) {
                return Err(Error::Query("The statements differ from the ones reviewed (the table changed?). Nothing was saved; review them again.".into()));
            }
        }
        connection.apply_changes(table.clone(), editing.column_infos(), changes).await
    }

    async fn checked(&self, target: &Target, table: &TableInfo, changes: &[RowChange]) -> Result<(TableEditing, Vec<EditStatement>, Arc<Connection>)> {
        let connection = self.connection(target).await?;
        let structure = connection.describe_table(table.clone()).await?;
        let editing = TableEditing::new(table, &structure);
        editing.check(changes)?;
        let statements = connection.preview_changes(table, &editing.column_infos(), changes)?;
        Ok((editing, statements, connection))
    }
}
