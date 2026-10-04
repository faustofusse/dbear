//! Structure view: columns, keys, indexes, foreign keys and DDL from the `sys` catalog views.
//! SQL Server has no `SHOW CREATE TABLE`, so table DDL is rebuilt here, like SSMS's "Script Table as".

use crate::driver::Result;
use crate::model::*;

use super::{decode, flag, int, object_id, opt_text, text, Lease, MSSQL};

/// Every column matching `filter` (on `o` = sys.objects, `s` = sys.schemas, `c` = sys.columns),
/// ordered by schema, table and position. Rows are read by [`ColumnRow::from`].
pub(super) fn columns_query(filter: &str) -> String {
    format!(
        "select s.name, o.name, c.name, t.name,
                cast(c.max_length as int), cast(c.precision as int), cast(c.scale as int), c.is_nullable,
                cast(isnull(pk.key_ordinal, 0) as int),
                object_definition(c.default_object_id),
                cc.definition, cc.is_persisted,
                cast(idc.seed_value as nvarchar(40)), cast(idc.increment_value as nvarchar(40)),
                cast(ep.value as nvarchar(max))
         from sys.objects o
         join sys.schemas s on s.schema_id = o.schema_id
         join sys.columns c on c.object_id = o.object_id
         join sys.types t on t.user_type_id = c.user_type_id
         left join (
             select ic.object_id, ic.column_id, ic.key_ordinal
             from sys.indexes i
             join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id
             where i.is_primary_key = 1
         ) pk on pk.object_id = c.object_id and pk.column_id = c.column_id
         left join sys.computed_columns cc on cc.object_id = c.object_id and cc.column_id = c.column_id
         left join sys.identity_columns idc on idc.object_id = c.object_id and idc.column_id = c.column_id
         left join sys.extended_properties ep
           on ep.class = 1 and ep.major_id = c.object_id and ep.minor_id = c.column_id and ep.name = N'MS_Description'
         where {filter}
         order by s.name, o.name, c.column_id"
    )
}

/// One row of [`columns_query`].
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct ColumnRow {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub type_name: String,
    pub is_nullable: bool,
    /// Position in the primary key (1-based), 0 when not part of it.
    pub pk_ordinal: i64,
    pub default: Option<String>,
    /// `(expression, persisted)`.
    pub computed: Option<(String, bool)>,
    /// `(seed, increment)`.
    pub identity: Option<(String, String)>,
    pub comment: Option<String>,
}

impl From<&Vec<Value>> for ColumnRow {
    fn from(row: &Vec<Value>) -> Self {
        let n = |i| int(row, i).unwrap_or_default();
        Self {
            schema: text(row, 0),
            table: text(row, 1),
            name: text(row, 2),
            type_name: decode::column_type(&text(row, 3), n(4), n(5), n(6)),
            is_nullable: flag(row, 7),
            pk_ordinal: n(8),
            default: opt_text(row, 9),
            computed: opt_text(row, 10).map(|expr| (expr, flag(row, 11))),
            identity: opt_text(row, 12).map(|seed| (seed, opt_text(row, 13).unwrap_or_else(|| "1".into()))),
            comment: opt_text(row, 14).filter(|c| !c.is_empty()),
        }
    }
}

impl ColumnRow {
    pub fn info(&self) -> ColumnInfo {
        ColumnInfo {
            name: self.name.clone(),
            type_name: self.type_name.clone(),
            is_primary_key: self.pk_ordinal > 0,
            is_nullable: self.is_nullable,
        }
    }

    /// What the structure view shows as the default: identity, computed expression or default.
    fn default_value(&self) -> Option<String> {
        if let Some((seed, increment)) = &self.identity {
            return Some(format!("IDENTITY({seed},{increment})"));
        }
        if let Some((expr, persisted)) = &self.computed {
            return Some(format!("AS {expr}{}", if *persisted { " PERSISTED" } else { "" }));
        }
        self.default.clone()
    }

    /// The column's line in `CREATE TABLE`.
    fn definition(&self) -> String {
        let name = MSSQL.quote_ident(&self.name);
        if let Some((expr, persisted)) = &self.computed {
            return format!("{name} AS {expr}{}", if *persisted { " PERSISTED" } else { "" });
        }
        let mut line = format!("{name} {}", self.type_name);
        if let Some((seed, increment)) = &self.identity {
            line.push_str(&format!(" IDENTITY({seed},{increment})"));
        }
        line.push_str(if self.is_nullable { " NULL" } else { " NOT NULL" });
        if let Some(default) = &self.default {
            line.push_str(&format!(" DEFAULT {default}"));
        }
        line
    }
}

/// An index (or the primary key / a unique constraint, which are backed by one).
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct IndexRow {
    pub name: String,
    pub is_primary: bool,
    pub is_unique: bool,
    pub is_unique_constraint: bool,
    pub clustered: bool,
    /// `(column, descending)` in key order.
    pub keys: Vec<(String, bool)>,
    pub included: Vec<String>,
    pub filter: Option<String>,
}

impl IndexRow {
    fn key_list(&self) -> String {
        self.keys
            .iter()
            .map(|(c, desc)| format!("{}{}", MSSQL.quote_ident(c), if *desc { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn kind(&self) -> &'static str {
        if self.clustered { "CLUSTERED" } else { "NONCLUSTERED" }
    }

    /// `CREATE [UNIQUE] [NON]CLUSTERED INDEX …` for plain indexes; keys and unique constraints are
    /// part of the table definition instead.
    fn create_statement(&self, relation: &str) -> Option<String> {
        if self.is_primary || self.is_unique_constraint {
            return None;
        }
        let mut sql = format!(
            "CREATE {}{} INDEX {} ON {relation} ({})",
            if self.is_unique { "UNIQUE " } else { "" },
            self.kind(),
            MSSQL.quote_ident(&self.name),
            self.key_list()
        );
        if !self.included.is_empty() {
            let included: Vec<String> = self.included.iter().map(|c| MSSQL.quote_ident(c)).collect();
            sql.push_str(&format!(" INCLUDE ({})", included.join(", ")));
        }
        if let Some(filter) = &self.filter {
            sql.push_str(&format!(" WHERE {filter}"));
        }
        sql.push(';');
        Some(sql)
    }

    fn info(&self, relation: &str) -> IndexInfo {
        IndexInfo {
            name: self.name.clone(),
            columns: self.keys.iter().map(|(c, desc)| if *desc { format!("{c} DESC") } else { c.clone() }).collect(),
            is_unique: self.is_unique,
            is_primary: self.is_primary,
            definition: self.create_statement(relation),
        }
    }
}

/// Everything `CREATE TABLE` needs.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct TableDef {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnRow>,
    pub indexes: Vec<IndexRow>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// `(name, definition)`.
    pub checks: Vec<(String, String)>,
}

/// `CREATE TABLE` with its keys, foreign keys and checks, then the other indexes.
pub(super) fn table_ddl(t: &TableDef) -> String {
    let relation = MSSQL.quote_relation(&t.schema, &t.name);
    let mut lines: Vec<String> = t.columns.iter().map(ColumnRow::definition).collect();
    for index in t.indexes.iter().filter(|i| i.is_primary || i.is_unique_constraint) {
        lines.push(format!(
            "CONSTRAINT {} {} {} ({})",
            MSSQL.quote_ident(&index.name),
            if index.is_primary { "PRIMARY KEY" } else { "UNIQUE" },
            index.kind(),
            index.key_list()
        ));
    }
    for fk in &t.foreign_keys {
        let quoted = |cols: &[String]| cols.iter().map(|c| MSSQL.quote_ident(c)).collect::<Vec<_>>().join(", ");
        let mut line = format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            MSSQL.quote_ident(&fk.name),
            quoted(&fk.columns),
            MSSQL.quote_relation(&fk.referenced_schema, &fk.referenced_table),
            quoted(&fk.referenced_columns)
        );
        if fk.on_delete != "NO ACTION" {
            line.push_str(&format!(" ON DELETE {}", fk.on_delete));
        }
        if fk.on_update != "NO ACTION" {
            line.push_str(&format!(" ON UPDATE {}", fk.on_update));
        }
        lines.push(line);
    }
    for (name, definition) in &t.checks {
        lines.push(format!("CONSTRAINT {} CHECK {definition}", MSSQL.quote_ident(name)));
    }
    let mut ddl = format!("CREATE TABLE {relation} (\n    {}\n);", lines.join(",\n    "));
    for index in &t.indexes {
        if let Some(sql) = index.create_statement(&relation) {
            ddl.push('\n');
            ddl.push_str(&sql);
        }
    }
    ddl
}

pub(super) async fn describe(lease: &mut Lease<'_>, table: &TableInfo, is_view: bool) -> Result<TableStructure> {
    let object = object_id(table);
    let sql = format!(
        "{columns};
         select i.name, i.is_primary_key, i.is_unique, i.is_unique_constraint, cast(i.type as int), i.filter_definition,
                c.name, ic.is_descending_key, ic.is_included_column
         from sys.indexes i
         join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id
         join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id
         where i.object_id = {object} and i.type > 0
         order by i.is_primary_key desc, i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id;
         select fk.name, pc.name, rs.name, ro.name, rc.name,
                fk.update_referential_action_desc, fk.delete_referential_action_desc
         from sys.foreign_keys fk
         join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id
         join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id
         join sys.objects ro on ro.object_id = fkc.referenced_object_id
         join sys.schemas rs on rs.schema_id = ro.schema_id
         join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id
         where fk.parent_object_id = {object}
         order by fk.name, fkc.constraint_column_id;
         select name, definition from sys.check_constraints where parent_object_id = {object} order by name;
         select object_definition({object});
         select rs.name, ro.name, fk.name, pc.name, rc.name
         from sys.foreign_keys fk
         join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id
         join sys.objects ro on ro.object_id = fk.parent_object_id
         join sys.schemas rs on rs.schema_id = ro.schema_id
         join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id
         join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id
         where fk.referenced_object_id = {object}
         order by rs.name, ro.name, fk.name, fkc.constraint_column_id",
        columns = columns_query(&format!("o.object_id = {object}")),
    );
    let mut sets = lease.results(&sql).await?.into_iter();
    let mut next = || sets.next().unwrap_or_default();
    let columns: Vec<ColumnRow> = next().iter().map(ColumnRow::from).collect();

    let mut indexes: Vec<IndexRow> = Vec::new();
    for row in next() {
        let name = text(&row, 0);
        if indexes.last().is_none_or(|i| i.name != name) {
            indexes.push(IndexRow {
                name,
                is_primary: flag(&row, 1),
                is_unique: flag(&row, 2),
                is_unique_constraint: flag(&row, 3),
                // sys.indexes.type: 1 = clustered, 2 = nonclustered, 5/6 = columnstore…
                clustered: int(&row, 4) == Some(1),
                filter: opt_text(&row, 5),
                ..Default::default()
            });
        }
        let index = indexes.last_mut().expect("pushed above");
        if flag(&row, 8) {
            index.included.push(text(&row, 6));
        } else {
            index.keys.push((text(&row, 6), flag(&row, 7)));
        }
    }

    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for row in next() {
        let name = text(&row, 0);
        if foreign_keys.last().is_none_or(|f| f.name != name) {
            let action = |i| text(&row, i).replace('_', " ");
            foreign_keys.push(ForeignKeyInfo {
                name,
                columns: Vec::new(),
                referenced_schema: text(&row, 2),
                referenced_table: text(&row, 3),
                referenced_columns: Vec::new(),
                on_update: action(5),
                on_delete: action(6),
            });
        }
        let fk = foreign_keys.last_mut().expect("pushed above");
        fk.columns.push(text(&row, 1));
        fk.referenced_columns.push(text(&row, 4));
    }
    let checks: Vec<(String, String)> = next().iter().map(|r| (text(r, 0), text(r, 1))).collect();
    let module = next().first().and_then(|r| opt_text(r, 0));
    let mut referenced_by: Vec<ReferencingKey> = Vec::new();
    for row in next() {
        let (schema, table, name) = (text(&row, 0), text(&row, 1), text(&row, 2));
        if referenced_by.last().is_none_or(|k| (&k.schema, &k.table, &k.name) != (&schema, &table, &name)) {
            referenced_by.push(ReferencingKey { schema, table, name, columns: Vec::new(), referenced_columns: Vec::new() });
        }
        let key = referenced_by.last_mut().expect("pushed above");
        key.columns.push(text(&row, 3));
        key.referenced_columns.push(text(&row, 4));
    }

    let mut primary_key: Vec<&ColumnRow> = columns.iter().filter(|c| c.pk_ordinal > 0).collect();
    primary_key.sort_by_key(|c| c.pk_ordinal);
    let primary_key = primary_key.into_iter().map(|c| c.name.clone()).collect();
    let relation = MSSQL.quote_relation(&table.schema, &table.name);
    let def = TableDef { schema: table.schema.clone(), name: table.name.clone(), columns, indexes, foreign_keys, checks };
    let ddl = if is_view { module.map(|m| format!("{};", m.trim().trim_end_matches(';'))) } else { Some(table_ddl(&def)) };
    Ok(TableStructure {
        columns: def
            .columns
            .iter()
            .map(|c| ColumnDetail {
                name: c.name.clone(),
                type_name: c.type_name.clone(),
                is_nullable: c.is_nullable,
                default_value: c.default_value(),
                is_primary_key: c.pk_ordinal > 0,
                comment: c.comment.clone(),
            })
            .collect(),
        primary_key,
        indexes: def.indexes.iter().map(|i| i.info(&relation)).collect(),
        foreign_keys: def.foreign_keys,
        referenced_by,
        ddl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, type_name: &str, nullable: bool) -> ColumnRow {
        ColumnRow { name: name.into(), type_name: type_name.into(), is_nullable: nullable, ..Default::default() }
    }

    #[test]
    fn renders_table_ddl() {
        let def = TableDef {
            schema: "sales".into(),
            name: "orders".into(),
            columns: vec![
                ColumnRow { identity: Some(("1".into(), "1".into())), pk_ordinal: 1, ..column("id", "int", false) },
                ColumnRow { default: Some("((0))".into()), ..column("total", "decimal(10,2)", false) },
                ColumnRow { computed: Some(("([total]*(0.2))".into(), true)), ..column("tax", "decimal(12,3)", true) },
                column("customer_id", "int", true),
            ],
            indexes: vec![
                IndexRow { name: "PK_orders".into(), is_primary: true, is_unique: true, clustered: true, keys: vec![("id".into(), false)], ..Default::default() },
                IndexRow {
                    name: "ix_orders_total".into(),
                    keys: vec![("total".into(), true)],
                    included: vec!["customer_id".into()],
                    filter: Some("([total]>(0))".into()),
                    ..Default::default()
                },
            ],
            foreign_keys: vec![ForeignKeyInfo {
                name: "FK_orders_customers".into(),
                columns: vec!["customer_id".into()],
                referenced_schema: "dbo".into(),
                referenced_table: "customers".into(),
                referenced_columns: vec!["id".into()],
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
            }],
            checks: vec![("CK_total".into(), "([total]>=(0))".into())],
        };
        assert_eq!(
            table_ddl(&def),
            "CREATE TABLE [sales].[orders] (
    [id] int IDENTITY(1,1) NOT NULL,
    [total] decimal(10,2) NOT NULL DEFAULT ((0)),
    [tax] AS ([total]*(0.2)) PERSISTED,
    [customer_id] int NULL,
    CONSTRAINT [PK_orders] PRIMARY KEY CLUSTERED ([id]),
    CONSTRAINT [FK_orders_customers] FOREIGN KEY ([customer_id]) REFERENCES [dbo].[customers] ([id]) ON DELETE CASCADE,
    CONSTRAINT [CK_total] CHECK ([total]>=(0))
);
CREATE NONCLUSTERED INDEX [ix_orders_total] ON [sales].[orders] ([total] DESC) INCLUDE ([customer_id]) WHERE ([total]>(0));"
        );
        assert_eq!(def.columns[0].default_value().as_deref(), Some("IDENTITY(1,1)"));
        assert_eq!(def.columns[2].default_value().as_deref(), Some("AS ([total]*(0.2)) PERSISTED"));
        assert_eq!(def.indexes[1].info("[sales].[orders]").columns, ["total DESC"]);
    }
}
