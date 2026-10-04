//! Catalog SQL for libSQL and the mapping of its rows. The same queries as the SQLite driver
//! (`sqlite_master` and the `pragma_*` table-valued functions), sent over Hrana and batched so a
//! call is one HTTP round trip.

use super::hrana::{HValue, Stmt, StmtResult};
use crate::dialect::Dialect;
use crate::driver::{Error, Result};
use crate::keyset::{Keyset, SeekColumn};
use crate::model::*;

pub(crate) const SQLITE: Dialect = Dialect(DatabaseKind::Libsql);

/// Remote libSQL databases have a single schema; `ATTACH` isn't available over Hrana.
pub(crate) const MAIN: &str = "main";

pub(crate) fn list_tables() -> Stmt {
    Stmt::new(
        "select name, type from sqlite_master where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' order by name",
    )
}

pub(crate) fn schemas(result: StmtResult) -> Vec<Schema> {
    let tables = result
        .rows
        .into_iter()
        .filter_map(|row| {
            let name = row.first()?.as_text()?;
            let kind = if row.get(1).and_then(HValue::as_text).as_deref() == Some("view") { TableKind::View } else { TableKind::Table };
            // No row counts: Turso bills every row read, and `count(*)` reads the whole table.
            Some(TableInfo { schema: MAIN.into(), name, kind, estimated_row_count: None })
        })
        .collect();
    vec![Schema { name: MAIN.into(), tables }]
}

pub(crate) fn list_columns() -> Stmt {
    Stmt::new(
        "select m.name, ti.name, ti.type, ti.\"notnull\", ti.pk
         from sqlite_master m, pragma_table_info(m.name) ti
         where m.type in ('table', 'view') and m.name not like 'sqlite\\_%' escape '\\'
         order by m.name, ti.cid",
    )
}

pub(crate) fn columns(result: StmtResult) -> Vec<TableColumns> {
    let mut tables: Vec<TableColumns> = Vec::new();
    for row in result.rows {
        let text = |i: usize| row.get(i).and_then(HValue::as_text).unwrap_or_default();
        let int = |i: usize| row.get(i).and_then(HValue::as_i64).unwrap_or(0);
        let table = text(0);
        if tables.last().is_none_or(|t| t.table != table) {
            tables.push(TableColumns { schema: MAIN.into(), table, columns: Vec::new() });
        }
        let (not_null, pk) = (int(3) != 0, int(4));
        tables.last_mut().expect("pushed above").columns.push(ColumnInfo {
            name: text(1),
            type_name: text(2).to_lowercase(),
            is_primary_key: pk > 0,
            is_nullable: !not_null && pk == 0,
        });
    }
    tables
}

/// Statements for [`TableMeta`]: the table's `sqlite_master` row, then its columns.
pub(crate) fn table_meta_statements(table: &TableInfo) -> [Stmt; 2] {
    let schema = SQLITE.quote_ident(&table.schema);
    [
        Stmt::with_args(format!("select type, sql from {schema}.sqlite_master where name = ?1 and type in ('table', 'view')"), &[&table.name]),
        Stmt::with_args(
            "select name, type, \"notnull\", pk, dflt_value from pragma_table_info(?1, ?2) order by cid",
            &[&table.name, &table.schema],
        ),
    ]
}

pub(crate) struct TableMeta {
    pub kind: TableKind,
    pub columns: Vec<ColumnInfo>,
    pub defaults: Vec<Option<String>>,
    /// Primary key columns in key order.
    pub primary_key: Vec<String>,
    pub without_rowid: bool,
    /// Columns declared NOT NULL (primary key columns are reported non-nullable regardless).
    pub not_null: Vec<String>,
}

pub(crate) fn table_meta(table: &TableInfo, master: StmtResult, info: StmtResult) -> Result<TableMeta> {
    let Some(row) = master.rows.into_iter().next() else { return Err(Error::TableNotFound(table.qualified_name())) };
    let kind = if row.first().and_then(HValue::as_text).as_deref() == Some("view") { TableKind::View } else { TableKind::Table };
    let ddl = row.get(1).and_then(HValue::as_text).unwrap_or_default();

    let mut columns = Vec::new();
    let mut defaults = Vec::new();
    let mut keyed = Vec::new();
    let mut not_null = Vec::new();
    for row in info.rows {
        let text = |i: usize| row.get(i).and_then(HValue::as_text);
        let int = |i: usize| row.get(i).and_then(HValue::as_i64).unwrap_or(0);
        let (name, pk) = (text(0).unwrap_or_default(), int(3));
        if pk > 0 {
            keyed.push((pk, name.clone()));
        }
        if int(2) != 0 {
            not_null.push(name.clone());
        }
        columns.push(ColumnInfo {
            name,
            type_name: text(1).unwrap_or_default().to_lowercase(),
            is_primary_key: pk > 0,
            is_nullable: int(2) == 0 && pk == 0,
        });
        defaults.push(text(4));
    }
    keyed.sort();
    Ok(TableMeta {
        kind,
        columns,
        defaults,
        primary_key: keyed.into_iter().map(|(_, name)| name).collect(),
        without_rowid: ddl.to_ascii_lowercase().contains("without rowid"),
        not_null,
    })
}

/// The order of a table's pages, as in the SQLite driver: the user's sort, then `rowid` (insertion
/// order for most tables and always indexed) or the primary key. Seeking needs that tiebreak to be
/// unique and not shadowed by a column of the same name; views always page with OFFSET.
pub(crate) fn keyset_for(meta: &TableMeta, table: &TableInfo, query: &RowQuery) -> Result<Keyset> {
    let width = meta.columns.len();
    let shadowed = meta.columns.iter().any(|c| ["rowid", "_rowid_", "oid"].iter().any(|r| c.name.eq_ignore_ascii_case(r)));
    let key_columns = || meta.primary_key.iter().filter_map(|c| SeekColumn::column(SQLITE, &meta.columns, c));
    let (tiebreak, enabled): (Vec<SeekColumn>, bool) = match meta.kind {
        TableKind::Table if !meta.without_rowid && meta.primary_key.len() != 1 => (vec![SeekColumn::row_id("rowid", width)], !shadowed),
        // A lone INTEGER PRIMARY KEY is the rowid itself. Other single keys can hold NULLs (an old
        // SQLite quirk), so `rowid` goes after them to keep the order unique.
        TableKind::Table if !meta.without_rowid && !meta.columns.iter().any(|c| c.is_primary_key && c.type_name == "integer") => {
            let mut keys: Vec<SeekColumn> = key_columns().map(|c| SeekColumn { nullable: true, ..c }).collect();
            keys.push(SeekColumn::row_id("rowid", width));
            (keys, !shadowed)
        }
        TableKind::Table => (key_columns().collect(), true),
        TableKind::View => (Vec::new(), false),
    };
    // Keys declared without NOT NULL may hold NULLs in SQLite: say so, so seeks handle them.
    let mut columns = meta.columns.clone();
    for c in &mut columns {
        if c.is_primary_key && meta.kind == TableKind::Table && !meta.without_rowid && c.type_name != "integer" {
            c.is_nullable = !meta.not_null.contains(&c.name);
        }
    }
    Keyset::new(SQLITE, table, query, &columns, tiebreak, enabled)
}

/// Indexes (with their columns and SQL), foreign keys, the DDL and the keys pointing at the
/// table, after [`table_meta_statements`].
pub(crate) fn structure_statements(table: &TableInfo) -> [Stmt; 4] {
    let schema = SQLITE.quote_ident(&table.schema);
    let args: &[&str] = &[&table.name, &table.schema];
    [
        Stmt::with_args(
            format!(
                "select il.name, il.\"unique\", il.origin, coalesce(ii.name, '<expression>'), m.sql
                 from pragma_index_list(?1, ?2) il
                 left join pragma_index_info(il.name, ?2) ii
                 left join {schema}.sqlite_master m on m.type = 'index' and m.name = il.name
                 order by il.origin = 'pk' desc, il.name, ii.seqno"
            ),
            args,
        ),
        Stmt::with_args(
            "select id, \"table\", \"from\", \"to\", on_update, on_delete from pragma_foreign_key_list(?1, ?2) order by id, seq",
            args,
        ),
        Stmt::with_args(
            format!(
                "select sql from {schema}.sqlite_master where tbl_name = ?1 and sql is not null
                 order by case type when 'table' then 0 when 'view' then 0 when 'index' then 1 else 2 end, name"
            ),
            &[&table.name],
        ),
        Stmt::with_args(crate::sqlite::referenced_by_query(&table.schema), args),
    ]
}

/// Builds the structure. Indexes, foreign keys, DDL and incoming keys are optional: a server that
/// doesn't support one of those pragmas (`None`) still gets the columns.
pub(crate) fn structure(
    table: &TableInfo, meta: TableMeta, indexes: Option<StmtResult>, foreign_keys: Option<StmtResult>, ddl: Option<StmtResult>,
    referenced_by: Option<StmtResult>,
) -> TableStructure {
    let columns = meta
        .columns
        .iter()
        .zip(meta.defaults)
        .map(|(c, default_value)| ColumnDetail {
            name: c.name.clone(),
            type_name: c.type_name.clone(),
            is_nullable: c.is_nullable,
            default_value,
            is_primary_key: c.is_primary_key,
            comment: None,
        })
        .collect();

    let mut index_list: Vec<IndexInfo> = Vec::new();
    for row in indexes.map(|r| r.rows).unwrap_or_default() {
        let text = |i: usize| row.get(i).and_then(HValue::as_text);
        let name = text(0).unwrap_or_default();
        if index_list.last().is_none_or(|i| i.name != name) {
            index_list.push(IndexInfo {
                name,
                columns: Vec::new(),
                is_unique: row.get(1).and_then(HValue::as_i64).unwrap_or(0) != 0,
                is_primary: text(2).as_deref() == Some("pk"),
                definition: text(4),
            });
        }
        if let Some(column) = text(3) {
            index_list.last_mut().expect("pushed above").columns.push(column);
        }
    }

    let mut fks: Vec<(i64, ForeignKeyInfo)> = Vec::new();
    for row in foreign_keys.map(|r| r.rows).unwrap_or_default() {
        let text = |i: usize| row.get(i).and_then(HValue::as_text);
        let id = row.first().and_then(HValue::as_i64).unwrap_or(0);
        if fks.last().is_none_or(|(last, _)| *last != id) {
            let fk = ForeignKeyInfo {
                name: String::new(),
                columns: Vec::new(),
                referenced_schema: table.schema.clone(),
                referenced_table: text(1).unwrap_or_default(),
                referenced_columns: Vec::new(),
                on_update: text(4).unwrap_or_default(),
                on_delete: text(5).unwrap_or_default(),
            };
            fks.push((id, fk));
        }
        let fk = &mut fks.last_mut().expect("pushed above").1;
        fk.columns.push(text(2).unwrap_or_default());
        if let Some(to) = text(3) {
            fk.referenced_columns.push(to);
        }
    }

    let statements: Vec<String> = ddl.map(|r| r.rows).unwrap_or_default().iter().filter_map(|r| r.first()?.as_text()).collect();
    let ddl = (!statements.is_empty())
        .then(|| statements.iter().map(|s| format!("{};", s.trim_end_matches(';'))).collect::<Vec<_>>().join("\n\n"));

    let incoming = referenced_by.map(|r| r.rows).unwrap_or_default().into_iter().map(|row| {
        let text = |i: usize| row.get(i).and_then(HValue::as_text);
        (text(0).unwrap_or_default(), row.get(1).and_then(HValue::as_i64).unwrap_or(0), text(2).unwrap_or_default(), text(3))
    });

    TableStructure {
        columns,
        primary_key: meta.primary_key,
        indexes: index_list,
        foreign_keys: fks.into_iter().map(|(_, fk)| fk).collect(),
        referenced_by: crate::sqlite::group_referenced_by(&table.schema, incoming.collect::<Vec<_>>()),
        ddl,
    }
}

/// Rows of a result, decoded by each column's declared type (or `declared` when the server has none).
pub(crate) fn decode_rows(result: &StmtResult, declared: &[ColumnInfo]) -> Vec<Vec<Value>> {
    let types: Vec<String> = (0..result.cols.len())
        .map(|i| match result.cols[i].decltype.as_deref() {
            Some(t) => t.to_lowercase(),
            None => declared.get(i).map(|c| c.type_name.clone()).unwrap_or_default(),
        })
        .collect();
    result
        .rows
        .iter()
        .map(|row| row.iter().enumerate().map(|(i, v)| v.decode(types.get(i).map_or("", String::as_str))).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> HValue {
        HValue::Text { value: s.into() }
    }
    fn int(i: i64) -> HValue {
        HValue::Integer { value: i.to_string() }
    }
    fn result(rows: Vec<Vec<HValue>>) -> StmtResult {
        StmtResult { rows, ..Default::default() }
    }

    #[test]
    fn maps_table_meta_and_keyset() {
        let table = TableInfo::new("main", "note_tags");
        let master = result(vec![vec![text("table"), text("create table note_tags (a, b, primary key (b, a)) WITHOUT ROWID")]]);
        let info = result(vec![
            vec![text("a"), text("INTEGER"), int(1), int(2), HValue::Null],
            vec![text("b"), text("INTEGER"), int(1), int(1), HValue::Null],
            vec![text("c"), text("TEXT"), int(0), int(0), text("'x'")],
        ]);
        let meta = table_meta(&table, master, info).unwrap();
        assert_eq!(meta.primary_key, ["b", "a"]);
        assert!(meta.without_rowid);
        assert_eq!(meta.not_null, ["a", "b"]);
        let keyset = keyset_for(&meta, &table, &RowQuery::default()).unwrap();
        assert_eq!(keyset.order_by(), ["\"b\"", "\"a\""]);
        assert!(keyset.enabled);
        assert!(meta.columns[2].is_nullable && !meta.columns[0].is_nullable);
        assert_eq!(meta.defaults[2].as_deref(), Some("'x'"));

        assert!(matches!(table_meta(&table, result(vec![]), result(vec![])), Err(Error::TableNotFound(_))));
    }

    #[test]
    fn hides_row_counts() {
        let schemas = schemas(result(vec![vec![text("a"), text("table")], vec![text("v"), text("view")]]));
        assert_eq!(schemas[0].tables.iter().map(|t| (t.kind, t.estimated_row_count)).collect::<Vec<_>>(), [
            (TableKind::Table, None),
            (TableKind::View, None)
        ]);
    }
}
