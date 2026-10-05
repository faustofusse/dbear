//! The data grid: a table's rows (or a script's result) from `dbcore`, shown with gpui-component's
//! virtualized table. Table rows page in as you scroll (keyset paging in the core, OFFSET where the
//! table has no usable key); header clicks sort and a `WHERE` filter narrows them, both on the
//! server. Copy formats come from `dbcore::export`, so they match the macOS app.

use std::sync::Arc;

use dbcore::export::{self, CopyFormat, Target};
use dbcore::{ColumnInfo, Connection, DatabaseKind, PageCursor, RowPage, RowQuery, SortKey, TableInfo, Value};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;

use crate::tabs::count;

/// Rows per page, like the macOS app.
pub const PAGE_SIZE: u32 = 500;

pub const COPY_FORMATS: [(CopyFormat, &str); 5] = [
    (CopyFormat::Tsv, "TSV"),
    (CopyFormat::Csv, "CSV"),
    (CopyFormat::Json, "JSON"),
    (CopyFormat::Markdown, "Markdown"),
    (CopyFormat::Insert, "INSERT statements"),
];

#[derive(Default)]
pub struct RowsDelegate {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Value>>,
    /// Table size when the driver knows it (first page only; with the filter applied).
    pub total: Option<u64>,
    pub loading_more: bool,
    pub load_more_error: Option<String>,
    /// Re-reading the first page after a sort or filter change.
    pub reloading: bool,
    /// Why the last sort/filter reload failed (the previous rows stay).
    pub reload_error: Option<String>,
    /// The sort and filter the rows were read with.
    pub query: RowQuery,
    /// For copying as `INSERT`s: the database kind and the table (none for script results).
    kind: Option<DatabaseKind>,
    source: Option<(Arc<Connection>, TableInfo)>,
    next: Option<PageCursor>,
    /// Bumped whenever the rows are replaced, so a page for the previous query is dropped.
    generation: u64,
    load_task: Option<Task<()>>,
}

impl RowsDelegate {
    /// Empties the grid.
    pub fn clear(&mut self) {
        self.generation += 1;
        self.columns.clear();
        self.rows.clear();
        self.total = None;
        self.source = None;
        self.next = None;
        self.loading_more = false;
        self.load_more_error = None;
        self.reloading = false;
        self.reload_error = None;
        self.query = RowQuery::default();
        self.load_task = None;
    }

    /// Shows a script's result: all of it at once, no paging or sorting.
    pub fn show_result(&mut self, kind: DatabaseKind, result: dbcore::QueryResult) {
        self.clear();
        self.kind = Some(kind);
        self.columns = result.columns;
        self.rows = result.rows;
        self.total = result.total_count;
    }

    /// Shows the first page of `table`; later pages load on scroll.
    pub fn show(&mut self, connection: Arc<Connection>, table: TableInfo, page: RowPage) {
        self.clear();
        self.kind = Some(connection.config().kind);
        self.columns = page.result.columns;
        self.rows = page.result.rows;
        self.total = page.result.total_count;
        self.next = page.next;
        self.source = Some((connection, table));
    }

    /// The table's rows can be sorted and filtered (script results can't).
    pub fn is_table(&self) -> bool {
        self.source.is_some()
    }

    /// Reads the first page again with `query` (a new sort or filter).
    pub fn reload(&mut self, query: RowQuery, cx: &mut Context<TableState<Self>>) {
        let Some((connection, table)) = self.source.clone() else { return };
        self.generation += 1;
        let generation = self.generation;
        self.query = query.clone();
        self.reloading = true;
        self.reload_error = None;
        self.loading_more = false;
        self.load_more_error = None;
        self.load_task = Some(cx.spawn(async move |state, cx| {
            let result = connection.fetch_page(table, query, PAGE_SIZE, None).await;
            state
                .update(cx, |state, cx| {
                    let rows = state.delegate_mut();
                    if rows.generation != generation {
                        return;
                    }
                    rows.reloading = false;
                    match result {
                        Ok(page) => {
                            rows.rows = page.result.rows;
                            rows.total = page.result.total_count;
                            rows.next = page.next;
                            if !rows.rows.is_empty() {
                                state.scroll_to_row(0, cx);
                            }
                        }
                        Err(e) => rows.reload_error = Some(e.to_string()),
                    }
                    cx.notify();
                })
                .ok();
        }));
        cx.notify();
    }

    /// Rows touched by the table's selection (a row, or a cell's row).
    pub fn selected_row(selection: TableSelection) -> Option<usize> {
        match selection {
            TableSelection::Row(row) | TableSelection::Cell(row, _) => Some(row),
            _ => None,
        }
    }

    /// `rows` as text in `format`, the way every dbear frontend copies them.
    pub fn format(&self, rows: &[usize], format: CopyFormat, headers: bool) -> String {
        let picked: Vec<Vec<Value>> = rows.iter().filter_map(|&ix| self.rows.get(ix).cloned()).collect();
        let (schema, table) = match &self.source {
            Some((_, table)) => (Some(table.schema.as_str()), Some(table.name.as_str())),
            None => (None, None),
        };
        export::format_rows(format, Target { kind: self.kind.unwrap_or(DatabaseKind::Postgres), schema, table }, &self.columns, &picked, headers)
    }

    /// A single value as text (NULL is empty).
    pub fn value_text(&self, row: usize, col: usize) -> Option<String> {
        let value = self.rows.get(row)?.get(col)?;
        Some(if value.is_null() { String::new() } else { value.display() })
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

pub fn copy(text: String, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
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
        let mut column = Column::new(format!("c{col_ix}"), info.name.clone()).width(px(width(info))).resizable(true);
        if self.is_table() {
            let sort = match self.query.sort.first() {
                Some(key) if key.column == info.name && key.descending => ColumnSort::Descending,
                Some(key) if key.column == info.name => ColumnSort::Ascending,
                _ => ColumnSort::Default,
            };
            column = column.sort(sort);
        }
        if self.is_numeric(col_ix) { column.text_right() } else { column }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let Some(column) = self.columns.get(col_ix) else { return };
        let mut query = self.query.clone();
        query.sort = match sort {
            ColumnSort::Default => Vec::new(),
            ColumnSort::Ascending => vec![SortKey { column: column.name.clone(), descending: false }],
            ColumnSort::Descending => vec![SortKey { column: column.name.clone(), descending: true }],
        };
        self.reload(query, cx);
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
        self.value_text(row_ix, col_ix).unwrap_or_default()
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let state = cx.entity().downgrade();
        // The cell under the selection when it's on this row, else none.
        let cell = match cx.entity().read(cx).selection() {
            TableSelection::Cell(row, col) if row == row_ix => Some(col),
            _ => None,
        };
        let read = move |cx: &mut App, f: &dyn Fn(&RowsDelegate) -> Option<String>| {
            if let Some(text) = state.upgrade().and_then(|s| f(s.read(cx).delegate())) {
                copy(text, cx);
            }
        };
        let mut menu = menu;
        if let Some(col) = cell {
            let read = read.clone();
            menu = menu.item(PopupMenuItem::new("Copy Value").on_click(move |_, _, cx| {
                read(cx, &|grid| grid.value_text(row_ix, col))
            }));
        }
        let (row_copy, headers_copy) = (read.clone(), read.clone());
        menu = menu
            .item(PopupMenuItem::new("Copy Row").on_click(move |_, _, cx| {
                row_copy(cx, &|grid| Some(grid.format(&[row_ix], CopyFormat::Tsv, false)))
            }))
            .item(PopupMenuItem::new("Copy Row with Headers").on_click(move |_, _, cx| {
                headers_copy(cx, &|grid| Some(grid.format(&[row_ix], CopyFormat::Tsv, true)))
            }));
        let row_as = read.clone();
        let row_menu = PopupMenu::build(window, cx, move |mut sub, _, _| {
            for (format, title) in COPY_FORMATS {
                let read = row_as.clone();
                sub = sub.item(PopupMenuItem::new(title).on_click(move |_, _, cx| {
                    read(cx, &|grid| Some(grid.format(&[row_ix], format, true)))
                }));
            }
            sub
        });
        let all_as = read.clone();
        let all_menu = PopupMenu::build(window, cx, move |mut sub, _, _| {
            for (format, title) in COPY_FORMATS {
                let read = all_as.clone();
                sub = sub.item(PopupMenuItem::new(title).on_click(move |_, _, cx| {
                    read(cx, &|grid| Some(grid.format(&(0..grid.rows.len()).collect::<Vec<_>>(), format, true)))
                }));
            }
            sub
        });
        let loaded = count(self.rows.len() as u64);
        menu.item(PopupMenuItem::submenu("Copy Row As", row_menu))
            .item(PopupMenuItem::submenu(format!("Copy All {loaded} Loaded Rows As"), all_menu))
    }

    fn has_more(&self, _: &App) -> bool {
        self.next.is_some() && !self.loading_more && !self.reloading && self.load_more_error.is_none()
    }

    fn load_more_threshold(&self) -> usize {
        100
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let (Some((connection, table)), Some(cursor)) = (self.source.clone(), self.next.clone()) else { return };
        if self.loading_more || self.reloading {
            return;
        }
        self.loading_more = true;
        let generation = self.generation;
        let query = self.query.clone();
        self.load_task = Some(cx.spawn(async move |state, cx| {
            let result = connection.fetch_page(table, query, PAGE_SIZE, Some(cursor)).await;
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
}
