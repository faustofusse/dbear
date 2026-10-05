//! The data grid: one page of rows from `dbcore`, shown with gpui-component's virtualized table.

use dbcore::{ColumnInfo, Value};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;

#[derive(Default)]
pub struct RowsDelegate {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
}

impl RowsDelegate {
    pub fn set(&mut self, columns: Vec<ColumnInfo>, rows: Vec<Vec<Value>>) {
        self.columns = columns;
        self.rows = rows;
    }

    fn is_numeric(&self, col_ix: usize) -> bool {
        let numeric = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_) | Value::Decimal(_));
        self.rows.iter().map(|row| &row[col_ix]).find(|v| !v.is_null()).is_some_and(numeric)
    }
}

/// A rough starting width from the type, like the macOS grid.
fn width(column: &ColumnInfo) -> f32 {
    let t = column.type_name.to_lowercase();
    if t.contains("bool") || t == "int2" || t == "smallint" {
        80.
    } else if t.contains("int") || t.contains("serial") {
        100.
    } else if t.contains("uuid") {
        290.
    } else if t.contains("time") || t.contains("date") {
        190.
    } else {
        180.
    }
}

impl TableDelegate for RowsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let info = &self.columns[col_ix];
        let column = Column::new(format!("c{col_ix}"), info.name.clone()).width(px(width(info))).resizable(true);
        if self.is_numeric(col_ix) { column.text_right() } else { column }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let value = &self.rows[row_ix][col_ix];
        let cell = h_flex().size_full().overflow_hidden().whitespace_nowrap().text_ellipsis();
        match value {
            Value::Null => cell.text_color(cx.theme().muted_foreground).italic().child("NULL"),
            Value::Int(_) | Value::Float(_) | Value::Decimal(_) => cell.justify_end().child(value.display()),
            // One line per cell: newlines would otherwise grow the row.
            _ => cell.child(value.display().replace(['\n', '\r'], " ")),
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        match &self.rows[row_ix][col_ix] {
            Value::Null => String::new(),
            value => value.display(),
        }
    }
}
