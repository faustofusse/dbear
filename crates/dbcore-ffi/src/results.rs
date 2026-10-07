//! Script results that can link and be edited like a table's rows (`dbcore::results`).

use std::sync::Arc;

use dbcore::results as core;

use crate::{ColumnInfo, Connection, DbError, EditStatement, EditValue, TableInfo, TableStructure, Value};

/// The table column a result column reads.
#[derive(uniffi::Record, Clone)]
pub struct ColumnOrigin {
    pub schema: String,
    pub table: String,
    pub column: String,
}

/// A foreign key whose columns are all in the result (`columns`: result column indexes).
#[derive(uniffi::Record)]
pub struct ForeignKeyLink {
    pub columns: Vec<u32>,
    pub schema: String,
    pub table: String,
    /// Empty: the target's primary key (SQLite's implicit reference).
    pub target_columns: Vec<String>,
    pub label: String,
}

/// Another table's key pointing at a result table (`values`: result columns it matches).
#[derive(uniffi::Record)]
pub struct ReferenceLink {
    pub values: Vec<u32>,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub label: String,
}

#[derive(uniffi::Record)]
pub struct ResultCellEdit {
    pub column: u32,
    pub value: EditValue,
}

/// One result row's edits: its values as loaded, new values for some cells, or delete it.
#[derive(uniffi::Record)]
pub struct RowEdit {
    pub values: Vec<Value>,
    pub set: Vec<ResultCellEdit>,
    pub delete: bool,
}

/// What a result's columns are, from the tables they read. Made by `Connection::describe_result`
/// (scripts) or `table_result_sources` (a table's own rows).
#[derive(uniffi::Object)]
pub struct ResultSources {
    pub(crate) inner: core::ResultSources,
}

#[uniffi::export]
impl ResultSources {
    /// Why cells of result column `column` can't be edited (`None`: they can).
    pub fn read_only_reason(&self, column: u32) -> Option<String> {
        self.inner.read_only_reason(column as usize)
    }

    /// Why no cell can be edited (`None`: some can).
    pub fn summary_reason(&self) -> Option<String> {
        self.inner.summary_reason()
    }

    pub fn can_delete_rows(&self) -> bool {
        self.inner.can_delete_rows()
    }

    /// The tables edits are saved to (for the review sheet's title).
    pub fn editable_tables(&self) -> Vec<String> {
        self.inner.tables.iter().filter(|t| t.read_only.is_none()).map(|t| t.table.name.clone()).collect()
    }

    pub fn foreign_keys(&self) -> Vec<ForeignKeyLink> {
        self.inner
            .foreign_keys
            .iter()
            .map(|l| ForeignKeyLink {
                columns: l.columns.iter().map(|&i| i as u32).collect(),
                schema: l.schema.clone(),
                table: l.table.clone(),
                target_columns: l.target_columns.clone(),
                label: l.label.clone(),
            })
            .collect()
    }

    pub fn referenced_by(&self) -> Vec<ReferenceLink> {
        self.inner
            .referenced_by
            .iter()
            .map(|l| ReferenceLink {
                values: l.values.iter().map(|&i| i as u32).collect(),
                schema: l.schema.clone(),
                table: l.table.clone(),
                columns: l.columns.clone(),
                label: l.label.clone(),
            })
            .collect()
    }
}

/// A table's own rows: every column is the table's (links like a script result's).
#[uniffi::export]
pub fn table_result_sources(table: TableInfo, columns: Vec<ColumnInfo>, structure: TableStructure) -> Arc<ResultSources> {
    let columns: Vec<dbcore::ColumnInfo> = columns.into_iter().map(Into::into).collect();
    Arc::new(ResultSources { inner: core::ResultSources::for_table(&table.into(), &columns, &structure.into()) })
}

fn row_edits(edits: Vec<RowEdit>) -> Vec<core::RowEdit> {
    edits
        .into_iter()
        .map(|e| core::RowEdit {
            values: e.values.into_iter().map(Into::into).collect(),
            set: e
                .set
                .into_iter()
                .map(|c| {
                    let value = match c.value {
                        EditValue::Null => dbcore::edit::EditValue::Null,
                        EditValue::Default => dbcore::edit::EditValue::Default,
                        EditValue::Text { text } => dbcore::edit::EditValue::Text(text),
                    };
                    (c.column as usize, value)
                })
                .collect(),
            delete: e.delete,
        })
        .collect()
}

#[uniffi::export]
impl Connection {
    /// What a script result's columns are (describes the tables they read).
    pub async fn describe_result(&self, origins: Vec<Option<ColumnOrigin>>) -> Result<Arc<ResultSources>, DbError> {
        let origins = origins
            .into_iter()
            .map(|o| o.map(|o| dbcore::ColumnOrigin { schema: o.schema, table: o.table, column: o.column }))
            .collect();
        Ok(Arc::new(ResultSources { inner: self.inner.describe_result(origins).await? }))
    }

    /// The statements `apply_result_edits` would run (for the review sheet).
    pub fn preview_result_edits(&self, sources: Arc<ResultSources>, edits: Vec<RowEdit>) -> Result<Vec<EditStatement>, DbError> {
        let changes = sources.inner.changes(&row_edits(edits))?;
        Ok(self.inner.preview_result_changes(&changes)?.into_iter().map(Into::into).collect())
    }

    /// Saves edits made in a script's results in one transaction. Returns the rows affected.
    pub async fn apply_result_edits(&self, sources: Arc<ResultSources>, edits: Vec<RowEdit>) -> Result<u64, DbError> {
        let changes = sources.inner.changes(&row_edits(edits))?;
        Ok(self.inner.apply_result_changes(changes).await?)
    }
}

impl From<TableStructure> for dbcore::TableStructure {
    fn from(s: TableStructure) -> Self {
        Self {
            columns: s
                .columns
                .into_iter()
                .map(|c| dbcore::ColumnDetail {
                    name: c.name,
                    type_name: c.type_name,
                    is_nullable: c.is_nullable,
                    default_value: c.default_value,
                    is_primary_key: c.is_primary_key,
                    comment: c.comment,
                })
                .collect(),
            primary_key: s.primary_key,
            indexes: s
                .indexes
                .into_iter()
                .map(|i| dbcore::IndexInfo {
                    name: i.name,
                    columns: i.columns,
                    is_unique: i.is_unique,
                    is_primary: i.is_primary,
                    definition: i.definition,
                })
                .collect(),
            foreign_keys: s
                .foreign_keys
                .into_iter()
                .map(|f| dbcore::ForeignKeyInfo {
                    name: f.name,
                    columns: f.columns,
                    referenced_schema: f.referenced_schema,
                    referenced_table: f.referenced_table,
                    referenced_columns: f.referenced_columns,
                    on_update: f.on_update,
                    on_delete: f.on_delete,
                })
                .collect(),
            referenced_by: s
                .referenced_by
                .into_iter()
                .map(|k| dbcore::ReferencingKey {
                    schema: k.schema,
                    table: k.table,
                    name: k.name,
                    columns: k.columns,
                    referenced_columns: k.referenced_columns,
                })
                .collect(),
            ddl: s.ddl,
        }
    }
}
