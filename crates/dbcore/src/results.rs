//! Script results that read table columns unchanged can link and be edited like a table tab.
//!
//! Drivers report where each result column comes from ([`QueryResult::origins`]). With the
//! structures of those tables, [`ResultSources`] says, by result column index:
//! - which cells can be edited: their table's whole primary key is in the result, once;
//! - which foreign keys can be followed: all of the key's columns are in the result;
//! - which tables reference a row: the referenced columns are in the result.
//!
//! Table tabs use [`ResultSources::for_table`], so both kinds of grid share the links.
//!
//! [`QueryResult::origins`]: crate::QueryResult::origins

use crate::driver::{Error, Result};
use crate::edit::{self, CellEdit, EditStatement, EditValue, KeyValue, RowChange};
use crate::model::{ColumnInfo, ColumnOrigin, DatabaseKind, TableInfo, TableKind, TableStructure, Value};

/// A table some result columns come from.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceTable {
    pub table: TableInfo,
    /// The table's columns, primary key flagged (what [`edit::statements`] checks against).
    pub columns: Vec<ColumnInfo>,
    /// Result columns holding its primary key, in key order. Empty when it can't be edited.
    pub key: Vec<usize>,
    /// Why its cells can't be edited (`None`: they can).
    pub read_only: Option<String>,
}

/// A foreign key whose columns are all in the result: a cell in it opens the row it points at.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKeyLink {
    /// Result columns holding the key, in the key's column order.
    pub columns: Vec<usize>,
    pub schema: String,
    pub table: String,
    /// The columns of `table` they match. Empty: its primary key (an implicit SQLite reference).
    pub target_columns: Vec<String>,
    /// For menus: "Open users Row", naming the columns when two keys point at the same table.
    pub label: String,
}

/// Another table's foreign key pointing at a table in the result: a row opens the rows referencing it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceLink {
    /// Result columns holding the values the key references (the source table's key, usually).
    pub values: Vec<usize>,
    /// The referencing table and its key's columns.
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    /// For menus: "orders (user_id)", plus "→ users" when several result tables have references.
    pub label: String,
}

/// What the result's columns are, as far as the tables they come from tell.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSources {
    pub tables: Vec<SourceTable>,
    /// Per result column: its table (index into `tables`) and its name there.
    pub columns: Vec<Option<(usize, String)>>,
    pub foreign_keys: Vec<ForeignKeyLink>,
    pub referenced_by: Vec<ReferenceLink>,
}

/// One result row's edits: its values as loaded, and the new values of some cells (or delete it).
#[derive(Debug, Clone, PartialEq)]
pub struct RowEdit {
    pub values: Vec<Value>,
    /// Result column → new value.
    pub set: Vec<(usize, EditValue)>,
    pub delete: bool,
}

/// Changes to one table, ready for [`edit::statements`].
#[derive(Debug, Clone, PartialEq)]
pub struct TableChanges {
    pub table: TableInfo,
    pub columns: Vec<ColumnInfo>,
    pub changes: Vec<RowChange>,
}

/// The tables `origins` read, each once, in the order they first appear (to describe them).
pub fn tables_to_describe(origins: &[Option<ColumnOrigin>]) -> Vec<TableInfo> {
    let mut tables: Vec<TableInfo> = Vec::new();
    for origin in origins.iter().flatten() {
        if !tables.iter().any(|t| t.schema == origin.schema && t.name == origin.table) {
            tables.push(TableInfo::new(origin.schema.clone(), origin.table.clone()));
        }
    }
    tables
}

/// The statements that save `changes`, table after table. Run them in one transaction.
pub fn statements(kind: DatabaseKind, changes: &[TableChanges]) -> Result<Vec<EditStatement>> {
    let mut all = Vec::new();
    for t in changes {
        all.extend(edit::statements(kind, &t.table, &t.columns, &t.changes)?);
    }
    Ok(all)
}

impl ResultSources {
    /// `origins`: per result column, as the driver reported it. `described`: the structures of
    /// [`tables_to_describe`] (a table missing here, e.g. it couldn't be read, is left out).
    pub fn new(origins: &[Option<ColumnOrigin>], described: &[(TableInfo, TableStructure)]) -> Self {
        let mut sources = Self { columns: vec![None; origins.len()], ..Self::default() };
        // Which source table each link came from, for the labels below.
        let mut reference_sources = Vec::new();
        for (table, structure) in described {
            let mine: Vec<(usize, &str)> = origins
                .iter()
                .enumerate()
                .filter_map(|(i, o)| o.as_ref().filter(|o| o.schema == table.schema && o.table == table.name).map(|o| (i, o.column.as_str())))
                .collect();
            if mine.is_empty() {
                continue;
            }
            let index = sources.tables.len();
            for &(i, name) in &mine {
                sources.columns[i] = Some((index, name.to_string()));
            }
            // `select a.id, b.id from t a join t b`: which row a value belongs to is lost.
            let twice = mine.iter().enumerate().any(|(j, (_, name))| mine[..j].iter().any(|(_, other)| other == name));
            let find = |name: &str| mine.iter().find(|(_, n)| *n == name).map(|(i, _)| *i);
            let find_all = |names: &[String]| names.iter().map(|n| find(n)).collect::<Option<Vec<usize>>>();

            let primary_key = &structure.primary_key;
            let key = find_all(primary_key);
            let read_only = if table.kind == TableKind::View {
                Some(format!("“{}” is a view.", table.name))
            } else if primary_key.is_empty() {
                Some(format!("“{}” has no primary key, so its rows can’t be identified for editing.", table.name))
            } else if twice {
                Some(format!("“{}” appears more than once in the results, so its rows can’t be told apart.", table.name))
            } else if key.is_none() {
                Some(format!("Include the primary key of “{}” ({}) in the results to edit its rows.", table.name, primary_key.join(", ")))
            } else {
                None
            };
            sources.tables.push(SourceTable {
                table: table.clone(),
                columns: structure
                    .columns
                    .iter()
                    .map(|c| ColumnInfo {
                        name: c.name.clone(),
                        type_name: c.type_name.clone(),
                        is_primary_key: c.is_primary_key || primary_key.contains(&c.name),
                        is_nullable: c.is_nullable,
                    })
                    .collect(),
                key: if read_only.is_none() { key.clone().unwrap_or_default() } else { Vec::new() },
                read_only,
            });
            if twice {
                continue;
            }

            for fk in &structure.foreign_keys {
                let Some(columns) = find_all(&fk.columns) else { continue };
                sources.foreign_keys.push(ForeignKeyLink {
                    columns,
                    schema: fk.referenced_schema.clone(),
                    table: fk.referenced_table.clone(),
                    target_columns: fk.referenced_columns.clone(),
                    label: format!("Open {} Row", fk.referenced_table),
                });
            }
            for r in &structure.referenced_by {
                // An implicit reference (SQLite) points at the primary key.
                let own = if r.referenced_columns.is_empty() { primary_key } else { &r.referenced_columns };
                if own.is_empty() {
                    continue;
                }
                let Some(values) = find_all(own) else { continue };
                let name = if r.schema == table.schema { r.table.clone() } else { format!("{}.{}", r.schema, r.table) };
                sources.referenced_by.push(ReferenceLink {
                    values,
                    schema: r.schema.clone(),
                    table: r.table.clone(),
                    columns: r.columns.clone(),
                    label: format!("{name} ({})", r.columns.join(", ")),
                });
                reference_sources.push(index);
            }
        }

        // Two keys to the same table: say which columns each one follows.
        let links = sources.foreign_keys.clone();
        for link in &mut sources.foreign_keys {
            if links.iter().filter(|l| l.schema == link.schema && l.table == link.table).count() > 1 {
                let names: Vec<String> = link.columns.iter().filter_map(|&i| sources.columns[i].as_ref().map(|(_, n)| n.clone())).collect();
                link.label = format!("Open {} Row ({})", link.table, names.join(", "));
            }
        }
        // References to several tables of a join: say whose row each one is about.
        if reference_sources.iter().any(|&t| t != reference_sources[0]) {
            for (link, &t) in sources.referenced_by.iter_mut().zip(&reference_sources) {
                link.label = format!("{} → {}", link.label, sources.tables[t].table.name);
            }
        }
        sources
    }

    /// A table tab's rows: every column is the table's own.
    pub fn for_table(table: &TableInfo, columns: &[ColumnInfo], structure: &TableStructure) -> Self {
        let origins: Vec<Option<ColumnOrigin>> = columns
            .iter()
            .map(|c| Some(ColumnOrigin { schema: table.schema.clone(), table: table.name.clone(), column: c.name.clone() }))
            .collect();
        Self::new(&origins, &[(table.clone(), structure.clone())])
    }

    /// Why the cells of result column `column` can't be edited (`None`: they can).
    pub fn read_only_reason(&self, column: usize) -> Option<String> {
        match self.columns.get(column) {
            Some(Some((table, _))) => self.tables[*table].read_only.clone(),
            _ => Some("This column is computed, not read from a table, so it can’t be edited.".into()),
        }
    }

    /// Some cell can be edited.
    pub fn is_editable(&self) -> bool {
        self.tables.iter().any(|t| t.read_only.is_none())
    }

    /// Rows can be deleted: every column comes from one table, which can be edited.
    pub fn can_delete_rows(&self) -> bool {
        self.tables.len() == 1 && self.tables[0].read_only.is_none() && self.columns.iter().all(Option::is_some)
    }

    /// Why a result's rows are read-only, for a message: `None` when some cells can be edited.
    pub fn summary_reason(&self) -> Option<String> {
        if self.is_editable() {
            return None;
        }
        Some(match self.tables.as_slice() {
            [] => "These results don’t read table columns directly, so they’re read-only.".into(),
            [only] => only.read_only.clone().unwrap_or_default(),
            _ => "None of the tables in these results can be edited: include their primary keys.".into(),
        })
    }

    /// The changes to save, grouped by table (tables without changes left out).
    pub fn changes(&self, edits: &[RowEdit]) -> Result<Vec<TableChanges>> {
        let mut per_table: Vec<Vec<RowChange>> = vec![Vec::new(); self.tables.len()];
        let key_of = |t: usize, values: &[Value]| -> Result<Vec<KeyValue>> {
            let table = &self.tables[t];
            let names = table.columns.iter().filter(|c| c.is_primary_key);
            table
                .key
                .iter()
                .zip(names.map(|c| c.name.clone()).collect::<Vec<_>>())
                .map(|(&i, column)| {
                    let value = values.get(i).cloned().ok_or_else(|| Error::Query("The row is missing its key.".into()))?;
                    Ok(KeyValue { column, value })
                })
                .collect()
        };
        for edit in edits {
            if edit.delete {
                if !self.can_delete_rows() {
                    return Err(Error::Unsupported("Rows can only be deleted from results that come from one table.".into()));
                }
                per_table[0].push(RowChange::Delete { key: key_of(0, &edit.values)? });
                continue;
            }
            let mut sets: Vec<Vec<CellEdit>> = vec![Vec::new(); self.tables.len()];
            for (column, value) in &edit.set {
                if let Some(reason) = self.read_only_reason(*column) {
                    return Err(Error::Unsupported(reason));
                }
                let (t, name) = self.columns[*column].clone().expect("checked above");
                sets[t].push(CellEdit { column: name, value: value.clone() });
            }
            for (t, set) in sets.into_iter().enumerate().filter(|(_, set)| !set.is_empty()) {
                per_table[t].push(RowChange::Update { key: key_of(t, &edit.values)?, set });
            }
        }
        Ok(self
            .tables
            .iter()
            .zip(per_table)
            .filter(|(_, changes)| !changes.is_empty())
            .map(|(t, changes)| TableChanges { table: t.table.clone(), columns: t.columns.clone(), changes })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDetail, ForeignKeyInfo, ReferencingKey};

    fn origin(table: &str, column: &str) -> Option<ColumnOrigin> {
        Some(ColumnOrigin { schema: "public".into(), table: table.into(), column: column.into() })
    }

    fn detail(name: &str, pk: bool) -> ColumnDetail {
        ColumnDetail { name: name.into(), type_name: "text".into(), is_nullable: !pk, default_value: None, is_primary_key: pk, comment: None }
    }

    fn users() -> (TableInfo, TableStructure) {
        let structure = TableStructure {
            columns: vec![detail("id", true), detail("name", false)],
            primary_key: vec!["id".into()],
            referenced_by: vec![ReferencingKey {
                schema: "public".into(),
                table: "orders".into(),
                name: "orders_user_id_fkey".into(),
                columns: vec!["user_id".into()],
                referenced_columns: vec!["id".into()],
            }],
            ..TableStructure::default()
        };
        (TableInfo::new("public", "users"), structure)
    }

    fn orders() -> (TableInfo, TableStructure) {
        let structure = TableStructure {
            columns: vec![detail("id", true), detail("user_id", false), detail("total", false)],
            primary_key: vec!["id".into()],
            foreign_keys: vec![ForeignKeyInfo {
                name: "orders_user_id_fkey".into(),
                columns: vec!["user_id".into()],
                referenced_schema: "public".into(),
                referenced_table: "users".into(),
                referenced_columns: vec!["id".into()],
                on_update: String::new(),
                on_delete: String::new(),
            }],
            ..TableStructure::default()
        };
        (TableInfo::new("public", "orders"), structure)
    }

    #[test]
    fn a_join_edits_each_table_by_its_own_key() {
        // select o.id, o.total, u.id, u.name, count(*) … from orders o join users u
        let origins = [origin("orders", "id"), origin("orders", "total"), origin("users", "id"), origin("users", "name"), None];
        let sources = ResultSources::new(&origins, &[orders(), users()]);
        assert_eq!(sources.tables.len(), 2);
        assert_eq!(sources.read_only_reason(1), None);
        assert_eq!(sources.read_only_reason(3), None);
        assert!(sources.read_only_reason(4).unwrap().contains("computed"));
        assert!(!sources.can_delete_rows());

        let values = vec![Value::Int(7), Value::Decimal("9.50".into()), Value::Int(3), Value::Text("Ada".into()), Value::Int(1)];
        let edits = [RowEdit { values, set: vec![(1, EditValue::Text("10".into())), (3, EditValue::Text("Grace".into()))], delete: false }];
        let changes = sources.changes(&edits).unwrap();
        let sql: Vec<String> = statements(DatabaseKind::Postgres, &changes).unwrap().into_iter().map(|s| s.sql).collect();
        assert_eq!(
            sql,
            [
                r#"UPDATE "public"."orders" SET "total" = '10' WHERE "id" = 7;"#,
                r#"UPDATE "public"."users" SET "name" = 'Grace' WHERE "id" = 3;"#
            ]
        );
    }

    #[test]
    fn without_the_primary_key_cells_are_read_only() {
        let sources = ResultSources::new(&[origin("users", "name")], &[users()]);
        let reason = sources.read_only_reason(0).unwrap();
        assert!(reason.contains("primary key of “users” (id)"), "{reason}");
        assert_eq!(sources.summary_reason(), Some(reason));
        assert!(sources.changes(&[RowEdit { values: vec![Value::Null], set: vec![(0, EditValue::Null)], delete: false }]).is_err());
    }

    #[test]
    fn a_self_join_is_read_only_and_unlinked() {
        let origins = [origin("users", "id"), origin("users", "id")];
        let sources = ResultSources::new(&origins, &[users()]);
        assert!(sources.read_only_reason(0).unwrap().contains("more than once"));
        assert!(sources.referenced_by.is_empty());
    }

    #[test]
    fn links_follow_keys_that_are_in_the_result() {
        let origins = [origin("orders", "id"), origin("orders", "user_id"), origin("users", "id")];
        let sources = ResultSources::new(&origins, &[orders(), users()]);
        assert_eq!(sources.foreign_keys.len(), 1);
        assert_eq!((sources.foreign_keys[0].columns.clone(), sources.foreign_keys[0].label.as_str()), (vec![1], "Open users Row"));
        // users.id is referenced by orders.user_id; only one result table has references.
        assert_eq!(sources.referenced_by[0].values, vec![2]);
        assert_eq!(sources.referenced_by[0].label, "orders (user_id)");
    }

    #[test]
    fn single_table_results_can_delete_rows() {
        let sources = ResultSources::new(&[origin("users", "id"), origin("users", "name")], &[users()]);
        assert!(sources.can_delete_rows());
        let changes = sources.changes(&[RowEdit { values: vec![Value::Int(3), Value::Null], set: vec![], delete: true }]).unwrap();
        let sql = statements(DatabaseKind::Postgres, &changes).unwrap();
        assert_eq!(sql[0].sql, r#"DELETE FROM "public"."users" WHERE "id" = 3;"#);
    }

    #[test]
    fn a_table_tab_links_like_before() {
        let (table, structure) = orders();
        let columns: Vec<ColumnInfo> = ["id", "user_id", "total"]
            .iter()
            .map(|n| ColumnInfo { name: (*n).into(), type_name: String::new(), is_primary_key: *n == "id", is_nullable: false })
            .collect();
        let sources = ResultSources::for_table(&table, &columns, &structure);
        assert_eq!(sources.foreign_keys[0].columns, vec![1]);
        assert_eq!(sources.read_only_reason(2), None);
    }
}
