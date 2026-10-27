//! Row edits for table tabs: changes are described as data ([`RowChange`]), turned into SQL here,
//! once for every dialect, and applied by the driver in a single transaction ([`Driver::apply`]).
//!
//! Values are written as SQL literals rather than bound parameters, so the review sheet shows
//! exactly the statements that run. New values are typed text quoted as string literals, which every
//! database casts to the column's type (`'42'` into an integer, `'{a,b}'` into a Postgres array…).
//!
//! [`Driver::apply`]: crate::Driver::apply

use crate::dialect::Dialect;
use crate::driver::{Error, Result};
use crate::model::{ColumnInfo, DatabaseKind, TableInfo, Value};

/// A new cell value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EditValue {
    Null,
    /// The column's default: `DEFAULT` in an UPDATE, the column left out of an INSERT.
    Default,
    /// Text as typed; the database converts it to the column's type.
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CellEdit {
    pub column: String,
    pub value: EditValue,
}

/// A primary key column and the row's current (original) value in it.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyValue {
    pub column: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Update { key: Vec<KeyValue>, set: Vec<CellEdit> },
    Insert { values: Vec<CellEdit> },
    Delete { key: Vec<KeyValue> },
}

/// One statement to run, and how to check it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditStatement {
    pub sql: String,
    /// UPDATE/DELETE by primary key: anything but exactly one row means the row changed or vanished.
    pub expect_one_row: bool,
    /// The row it targets, for error messages: `id = 5`.
    pub target: String,
}

/// The statements for `changes`, in a safe order: deletes, then updates, then inserts (so a row can
/// be replaced by a new one with the same unique values). `columns` are the table's columns
/// (`QueryResult::columns` of a table page), used to validate names and to spell booleans.
pub fn statements(kind: DatabaseKind, table: &TableInfo, columns: &[ColumnInfo], changes: &[RowChange]) -> Result<Vec<EditStatement>> {
    let d = Dialect(kind);
    let relation = d.quote_relation(&table.schema, &table.name);
    let column = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::Query(format!("“{}” has no column “{name}”", table.name)))
    };
    let key_clause = |key: &[KeyValue]| -> Result<String> {
        if key.is_empty() {
            return Err(Error::Unsupported(format!("“{}” has no primary key, so its rows can’t be edited", table.name)));
        }
        let mut terms = Vec::new();
        for k in key {
            let c = column(&k.column)?;
            if !c.is_primary_key {
                return Err(Error::Query(format!("“{}” isn’t part of the primary key", k.column)));
            }
            terms.push(format!("{} = {}", d.quote_ident(&k.column), key_literal(d, c, &k.value)?));
        }
        Ok(terms.join(" and "))
    };

    let mut deletes = Vec::new();
    let mut updates = Vec::new();
    let mut inserts = Vec::new();
    for change in changes {
        match change {
            RowChange::Delete { key } => {
                let target = key_clause(key)?;
                deletes.push(EditStatement { sql: format!("DELETE FROM {relation} WHERE {target};"), expect_one_row: true, target });
            }
            RowChange::Update { key, set } => {
                if set.is_empty() {
                    continue;
                }
                let target = key_clause(key)?;
                let assignments = set
                    .iter()
                    .map(|e| {
                        let c = column(&e.column)?;
                        let value = match &e.value {
                            EditValue::Default if kind.is_sqlite_family() => {
                                return Err(Error::Unsupported("SQLite can’t reset a column to its default in an UPDATE".into()));
                            }
                            EditValue::Default => "DEFAULT".to_string(),
                            other => value_literal(d, c, other),
                        };
                        Ok(format!("{} = {value}", d.quote_ident(&e.column)))
                    })
                    .collect::<Result<Vec<_>>>()?;
                updates.push(EditStatement {
                    sql: format!("UPDATE {relation} SET {} WHERE {target};", assignments.join(", ")),
                    expect_one_row: true,
                    target,
                });
            }
            RowChange::Insert { values } => {
                let given: Vec<&CellEdit> = values.iter().filter(|e| e.value != EditValue::Default).collect();
                let sql = if given.is_empty() {
                    match kind {
                        DatabaseKind::Mysql => format!("INSERT INTO {relation} () VALUES ();"),
                        _ => format!("INSERT INTO {relation} DEFAULT VALUES;"),
                    }
                } else {
                    let names = given.iter().map(|e| column(&e.column).map(|_| d.quote_ident(&e.column))).collect::<Result<Vec<_>>>()?;
                    let literals = given.iter().map(|e| Ok(value_literal(d, column(&e.column)?, &e.value))).collect::<Result<Vec<_>>>()?;
                    format!("INSERT INTO {relation} ({}) VALUES ({});", names.join(", "), literals.join(", "))
                };
                inserts.push(EditStatement { sql, expect_one_row: false, target: "a new row".into() });
            }
        }
    }
    Ok(deletes.into_iter().chain(updates).chain(inserts).collect())
}

/// Checks what an applied statement reported. Drivers call this inside their transaction and roll
/// back on error, so either every change is saved or none is.
pub(crate) fn check_affected(statement: &EditStatement, affected: u64) -> Result<()> {
    match affected {
        _ if !statement.expect_one_row => Ok(()),
        1 => Ok(()),
        0 => Err(Error::Query(format!(
            "No row matches {} any more: it was deleted or its key changed since it was loaded. Nothing was saved; reload and try again.",
            statement.target
        ))),
        n => Err(Error::Query(format!("{n} rows match {}, so it isn’t a unique key. Nothing was saved.", statement.target))),
    }
}

/// A statement failed on the server: say which change, and that the transaction was rolled back.
pub(crate) fn failed(statement: &EditStatement, error: Error) -> Error {
    match error {
        Error::Query(message) => Error::Query(format!("Couldn’t save {}:\n{message}\nNothing was saved.", describe(statement))),
        other => other,
    }
}

fn describe(statement: &EditStatement) -> String {
    if statement.expect_one_row { format!("the row where {}", statement.target) } else { statement.target.clone() }
}

/// MySQL and SQLite store booleans as integers, SQL Server as `bit`: `true` typed into one must become `1`.
fn is_boolean(kind: DatabaseKind, column: &ColumnInfo) -> bool {
    let t = column.type_name.to_ascii_lowercase();
    match kind {
        DatabaseKind::Postgres => false,
        DatabaseKind::Mysql => t == "tinyint(1)" || t == "boolean" || t == "bool",
        DatabaseKind::Sqlite | DatabaseKind::Libsql | DatabaseKind::D1 => t.contains("bool"),
        DatabaseKind::SqlServer => t == "bit",
    }
}

/// Binary values reach the grid as a hex preview (`0x…`, cut after 4 KB): they can't be written back.
pub fn is_binary(column: &ColumnInfo) -> bool {
    let t = column.type_name.to_ascii_lowercase();
    // SQL Server: `image`, `rowversion` (reported instead of its alias `timestamp`), CLR types.
    ["bytea", "blob", "binary", "geometry", "image", "rowversion", "geography", "hierarchyid"].iter().any(|b| t.contains(b))
}

fn value_literal(d: Dialect, column: &ColumnInfo, value: &EditValue) -> String {
    match value {
        EditValue::Null => "NULL".into(),
        EditValue::Default => "DEFAULT".into(),
        EditValue::Text(text) if is_boolean(d.0, column) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => "1".into(),
            "false" | "f" | "no" | "n" | "off" | "0" => "0".into(),
            _ => d.quote_literal(text),
        },
        EditValue::Text(text) => d.quote_literal(text),
    }
}

/// A row's current key value as a literal for `WHERE`: numbers bare, so they compare as numbers.
fn key_literal(d: Dialect, column: &ColumnInfo, value: &Value) -> Result<String> {
    if is_binary(column) {
        return Err(Error::Unsupported(format!("rows keyed by binary column “{}” can’t be edited", column.name)));
    }
    Ok(match value {
        Value::Null => return Err(Error::Query(format!("primary key “{}” is NULL", column.name))),
        Value::Bool(b) => match d.0 {
            DatabaseKind::Postgres => b.to_string(),
            _ => u8::from(*b).to_string(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
        Value::Float(_) | Value::Decimal(_) => d.quote_literal(&value.display()),
        Value::Text(s) => d.quote_literal(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<ColumnInfo> {
        let c = |name: &str, ty: &str, pk| ColumnInfo { name: name.into(), type_name: ty.into(), is_primary_key: pk, is_nullable: !pk };
        vec![c("id", "bigint", true), c("name", "text", false), c("active", "tinyint(1)", false), c("data", "bytea", false)]
    }

    fn key(id: i64) -> Vec<KeyValue> {
        vec![KeyValue { column: "id".into(), value: Value::Int(id) }]
    }

    fn set(column: &str, value: EditValue) -> CellEdit {
        CellEdit { column: column.into(), value }
    }

    fn sql(kind: DatabaseKind, changes: &[RowChange]) -> Result<Vec<String>> {
        Ok(statements(kind, &TableInfo::new("app", "users"), &columns(), changes)?.into_iter().map(|s| s.sql).collect())
    }

    #[test]
    fn builds_statements_in_safe_order() {
        let changes = [
            RowChange::Insert { values: vec![set("name", EditValue::Text("Ada".into())), set("id", EditValue::Default)] },
            RowChange::Update { key: key(2), set: vec![set("name", EditValue::Text("O'Neil".into())), set("active", EditValue::Null)] },
            RowChange::Delete { key: key(3) },
        ];
        assert_eq!(
            sql(DatabaseKind::Postgres, &changes).unwrap(),
            [
                r#"DELETE FROM "app"."users" WHERE "id" = 3;"#,
                r#"UPDATE "app"."users" SET "name" = 'O''Neil', "active" = NULL WHERE "id" = 2;"#,
                r#"INSERT INTO "app"."users" ("name") VALUES ('Ada');"#,
            ]
        );
        let all = statements(DatabaseKind::Postgres, &TableInfo::new("app", "users"), &columns(), &changes).unwrap();
        assert_eq!(all.iter().map(|s| s.expect_one_row).collect::<Vec<_>>(), [true, true, false]);
        assert_eq!(all[0].target, r#""id" = 3"#);
    }

    #[test]
    fn spells_values_per_dialect() {
        let update = [RowChange::Update { key: key(1), set: vec![set("active", EditValue::Text("true".into()))] }];
        assert_eq!(sql(DatabaseKind::Mysql, &update).unwrap(), ["UPDATE `app`.`users` SET `active` = 1 WHERE `id` = 1;"]);
        // Postgres has real booleans: the literal is cast by the server.
        assert_eq!(sql(DatabaseKind::Postgres, &update).unwrap(), [r#"UPDATE "app"."users" SET "active" = 'true' WHERE "id" = 1;"#]);

        let empty = [RowChange::Insert { values: vec![set("id", EditValue::Default)] }];
        assert_eq!(sql(DatabaseKind::Mysql, &empty).unwrap(), ["INSERT INTO `app`.`users` () VALUES ();"]);
        assert_eq!(sql(DatabaseKind::Sqlite, &empty).unwrap(), [r#"INSERT INTO "app"."users" DEFAULT VALUES;"#]);

        let reset = [RowChange::Update { key: key(1), set: vec![set("name", EditValue::Default)] }];
        assert_eq!(sql(DatabaseKind::Postgres, &reset).unwrap(), [r#"UPDATE "app"."users" SET "name" = DEFAULT WHERE "id" = 1;"#]);
        assert!(matches!(sql(DatabaseKind::Sqlite, &reset), Err(Error::Unsupported(_))));

        // SQL Server: `bit` takes 1/0, strings are Unicode literals.
        let ms = [RowChange::Update { key: key(1), set: vec![set("active", EditValue::Text("yes".into())), set("name", EditValue::Text("Zoë".into()))] }];
        let columns = [columns(), vec![ColumnInfo { name: "active".into(), type_name: "bit".into(), is_primary_key: false, is_nullable: true }]].concat();
        let columns: Vec<_> = columns.into_iter().filter(|c| c.type_name != "tinyint(1)").collect();
        let ms = statements(DatabaseKind::SqlServer, &TableInfo::new("dbo", "users"), &columns, &ms).unwrap();
        assert_eq!(ms[0].sql, "UPDATE [dbo].[users] SET [active] = 1, [name] = N'Zoë' WHERE [id] = 1;");
        assert_eq!(sql(DatabaseKind::SqlServer, &empty).unwrap(), ["INSERT INTO [app].[users] DEFAULT VALUES;"]);

        // MySQL strings escape backslashes.
        let path = [RowChange::Update { key: key(1), set: vec![set("name", EditValue::Text(r"C:\temp".into()))] }];
        assert_eq!(sql(DatabaseKind::Mysql, &path).unwrap(), [r"UPDATE `app`.`users` SET `name` = 'C:\\temp' WHERE `id` = 1;"]);
    }

    #[test]
    fn rejects_bad_targets() {
        let no_key = [RowChange::Delete { key: vec![] }];
        assert!(matches!(sql(DatabaseKind::Postgres, &no_key), Err(Error::Unsupported(_))));
        let not_pk = [RowChange::Delete { key: vec![KeyValue { column: "name".into(), value: Value::Text("x".into()) }] }];
        assert!(sql(DatabaseKind::Postgres, &not_pk).is_err());
        let unknown = [RowChange::Update { key: key(1), set: vec![set("nope", EditValue::Null)] }];
        assert!(sql(DatabaseKind::Postgres, &unknown).is_err());
        let null_key = [RowChange::Delete { key: vec![KeyValue { column: "id".into(), value: Value::Null }] }];
        assert!(sql(DatabaseKind::Postgres, &null_key).is_err());
        // Updates with nothing to set are skipped, not sent as invalid SQL.
        assert!(sql(DatabaseKind::Postgres, &[RowChange::Update { key: key(1), set: vec![] }]).unwrap().is_empty());
    }
}
