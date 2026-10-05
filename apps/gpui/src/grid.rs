//! The data grid: a table's rows from `dbcore`, shown with gpui-component's virtualized table.
//! The first page comes from the workspace; scrolling near the end loads the next one (keyset
//! paging in the core, OFFSET where the table has no usable key).

use std::sync::Arc;

use dbcore::{ColumnInfo, Connection, PageCursor, RowPage, RowQuery, TableInfo, Value};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;

/// Rows per page, like the macOS app.
pub const PAGE_SIZE: u32 = 500;

#[derive(Default)]
pub struct RowsDelegate {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    /// Table size when the driver knows it (first page only).
    pub total: Option<u64>,
    pub loading_more: bool,
    pub load_more_error: Option<String>,
    source: Option<(Arc<Connection>, TableInfo)>,
    next: Option<PageCursor>,
    /// Bumped whenever the rows are replaced, so a page for the previous table is dropped.
    generation: u64,
    load_more_task: Option<Task<()>>,
}

impl RowsDelegate {
    /// Empties the grid (no table, or a new one loading).
    pub fn clear(&mut self) {
        self.generation += 1;
        self.columns.clear();
        self.rows.clear();
        self.total = None;
        self.source = None;
        self.next = None;
        self.loading_more = false;
        self.load_more_error = None;
        self.load_more_task = None;
    }

    /// Shows a script's result: all of it at once, no paging.
    pub fn show_result(&mut self, result: dbcore::QueryResult) {
        self.clear();
        self.columns = result.columns;
        self.rows = result.rows;
        self.total = result.total_count;
    }

    /// Shows the first page of `table`; later pages load on scroll.
    pub fn show(&mut self, connection: Arc<Connection>, table: TableInfo, page: RowPage) {
        self.clear();
        self.columns = page.result.columns;
        self.rows = page.result.rows;
        self.total = page.result.total_count;
        self.next = page.next;
        self.source = Some((connection, table));
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
    fn has_more(&self, _: &App) -> bool {
        self.next.is_some() && !self.loading_more && self.load_more_error.is_none()
    }

    fn load_more_threshold(&self) -> usize {
        100
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let (Some((connection, table)), Some(cursor)) = (self.source.clone(), self.next.clone()) else { return };
        if self.loading_more {
            return;
        }
        self.loading_more = true;
        let generation = self.generation;
        self.load_more_task = Some(cx.spawn(async move |state, cx| {
            let result = connection.fetch_page(table, RowQuery::default(), PAGE_SIZE, Some(cursor)).await;
            state
                .update(cx, |state, cx| {
                    let rows = state.delegate_mut();
                    if rows.generation != generation {
                        return;
                    }
                    rows.loading_more = false;
                    match result {
                        Ok(page) => {
                            rows.rows.extend(page.result.rows);
                            rows.next = page.next;
                        }
                        Err(e) => rows.load_more_error = Some(e.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        }));
        cx.notify();
    }

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
