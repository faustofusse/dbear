//! The data grid: a table's rows (or a script's result) from `dbcore`, shown with gpui-component's
//! virtualized table. Table rows page in as you scroll (keyset paging in the core, OFFSET where the
//! table has no usable key); header clicks sort and a `WHERE` filter narrows them, both on the
//! server. Copy formats come from `dbcore::export`, so they match the macOS app.
//!
//! Edits are kept here until saved (`PendingEdits`): the cells show them, and the core turns them
//! into one transaction (`dbcore::edit`), like the macOS app.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use dbcore::export::{self, CopyFormat, Target};
use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange, is_binary};
use dbcore::{ColumnInfo, Connection, DatabaseKind, PageCursor, RowPage, RowQuery, SortKey, TableInfo, TableKind, Value};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex};
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

/// Unsaved changes to a table's rows. Row indexes are positions in the loaded rows; new rows come
/// after them.
#[derive(Default, Clone)]
pub struct PendingEdits {
    /// Row → column → new value.
    pub updates: BTreeMap<usize, BTreeMap<usize, EditValue>>,
    pub deleted: BTreeSet<usize>,
    /// One value per column; `Default` until typed in.
    pub inserted: Vec<Vec<EditValue>>,
}

impl PendingEdits {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    /// "2 edited · 1 new · 1 deleted".
    pub fn summary(&self) -> String {
        let edited = self.updates.keys().filter(|row| !self.deleted.contains(row)).count();
        let mut parts = Vec::new();
        if edited > 0 {
            parts.push(format!("{edited} edited"));
        }
        if !self.inserted.is_empty() {
            parts.push(format!("{} new", self.inserted.len()));
        }
        if !self.deleted.is_empty() {
            parts.push(format!("{} deleted", self.deleted.len()));
        }
        parts.join(" · ")
    }
}

/// Starts editing a cell (the table tab owns the input). Set by the tab, called from the menu.
pub type BeginEdit = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

#[derive(Default)]
pub struct RowsDelegate {
    pub edits: PendingEdits,
    /// The cell being edited and its input.
    pub editing: Option<(usize, usize, Entity<InputState>)>,
    pub begin_edit: Option<BeginEdit>,
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
        self.edits = PendingEdits::default();
        self.editing = None;
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
        // Reloading renumbers the rows the edits point at.
        if !self.edits.is_empty() {
            self.reload_error = Some("Save or discard your changes first.".into());
            cx.notify();
            return;
        }
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

    /// Reads the first page again with the current sort and filter (after saving).
    pub fn refresh_rows(&mut self, cx: &mut Context<TableState<Self>>) {
        let query = self.query.clone();
        self.reload(query, cx);
    }

    // MARK: editing

    /// Why these rows can't be edited, or `None` when they can.
    pub fn read_only_reason(&self) -> Option<String> {
        let Some((_, table)) = &self.source else { return Some("Script results are read-only.".into()) };
        if table.kind == TableKind::View {
            return Some("Views are read-only.".into());
        }
        if !self.columns.iter().any(|c| c.is_primary_key) {
            return Some(format!("“{}” has no primary key, so its rows can’t be identified for editing.", table.name));
        }
        None
    }

    pub fn is_editable(&self, col: usize) -> bool {
        self.read_only_reason().is_none() && self.columns.get(col).is_some_and(|c| !is_binary(c))
    }

    pub fn source(&self) -> Option<&(Arc<Connection>, TableInfo)> {
        self.source.as_ref()
    }

    /// The value a cell shows: the pending edit, else the loaded value (`None`: out of range).
    pub fn shown(&self, row: usize, col: usize) -> Option<EditValue> {
        if let Some(values) = row.checked_sub(self.rows.len()).and_then(|i| self.edits.inserted.get(i)) {
            return values.get(col).cloned();
        }
        if let Some(edit) = self.edits.updates.get(&row).and_then(|cells| cells.get(&col)) {
            return Some(edit.clone());
        }
        let value = self.rows.get(row)?.get(col)?;
        Some(if value.is_null() { EditValue::Null } else { EditValue::Text(value.display()) })
    }

    /// Text to start editing with (NULL and DEFAULT start empty).
    pub fn edit_text(&self, row: usize, col: usize) -> String {
        match self.shown(row, col) {
            Some(EditValue::Text(text)) => text,
            _ => String::new(),
        }
    }

    pub fn set_cell(&mut self, row: usize, col: usize, value: EditValue) {
        if let Some(values) = row.checked_sub(self.rows.len()).and_then(|i| self.edits.inserted.get_mut(i)) {
            if let Some(cell) = values.get_mut(col) {
                *cell = value;
            }
            return;
        }
        let Some(original) = self.rows.get(row).and_then(|r| r.get(col)) else { return };
        let unchanged = match (&value, original) {
            (EditValue::Null, v) => v.is_null(),
            (EditValue::Text(text), v) => !v.is_null() && *text == v.display(),
            (EditValue::Default, _) => false,
        };
        let cells = self.edits.updates.entry(row).or_default();
        if unchanged {
            cells.remove(&col);
        } else {
            cells.insert(col, value);
        }
        if cells.is_empty() {
            self.edits.updates.remove(&row);
        }
    }

    /// Adds an empty row (every column DEFAULT) and returns its index.
    pub fn add_row(&mut self) -> usize {
        self.edits.inserted.push(vec![EditValue::Default; self.columns.len()]);
        self.rows.len() + self.edits.inserted.len() - 1
    }

    /// Marks a loaded row for deletion (or restores it); a new row is just removed.
    pub fn toggle_delete(&mut self, row: usize) {
        if let Some(i) = row.checked_sub(self.rows.len()) {
            if i < self.edits.inserted.len() {
                self.edits.inserted.remove(i);
                self.editing = None;
            }
        } else if !self.edits.deleted.remove(&row) {
            self.edits.deleted.insert(row);
        }
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.edits.deleted.contains(&row)
    }

    pub fn discard_edits(&mut self) {
        self.edits = PendingEdits::default();
        self.editing = None;
    }

    /// The pending edits as core changes, keyed by each row's primary key as loaded.
    pub fn changes(&self) -> Vec<RowChange> {
        let key = |row: usize| -> Vec<KeyValue> {
            let Some(values) = self.rows.get(row) else { return Vec::new() };
            self.columns
                .iter()
                .zip(values)
                .filter(|(c, _)| c.is_primary_key)
                .map(|(c, v)| KeyValue { column: c.name.clone(), value: v.clone() })
                .collect()
        };
        let mut changes: Vec<RowChange> = self.edits.deleted.iter().map(|&row| RowChange::Delete { key: key(row) }).collect();
        for (&row, cells) in &self.edits.updates {
            if self.edits.deleted.contains(&row) {
                continue;
            }
            let set = cells.iter().map(|(&col, value)| CellEdit { column: self.columns[col].name.clone(), value: value.clone() }).collect();
            changes.push(RowChange::Update { key: key(row), set });
        }
        for values in &self.edits.inserted {
            let values = self.columns.iter().zip(values).map(|(c, v)| CellEdit { column: c.name.clone(), value: v.clone() }).collect();
            changes.push(RowChange::Insert { values });
        }
        changes
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

    /// A single value as shown, as text (NULL and DEFAULT are empty).
    pub fn value_text(&self, row: usize, col: usize) -> Option<String> {
        match self.shown(row, col)? {
            EditValue::Text(text) => Some(text),
            EditValue::Null | EditValue::Default => Some(String::new()),
        }
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
        self.rows.len() + self.edits.inserted.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let info = &self.columns[col_ix];
        // No table padding: cells pad their own content (`cell`), so edit tints fill the whole cell.
        let mut column =
            Column::new(format!("c{col_ix}"), info.name.clone()).width(px(width(info))).resizable(true).p_0();
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

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let name = self.columns.get(col_ix).map(|c| c.name.clone()).unwrap_or_default();
        let _ = cx;
        div().size_full().px_2().flex().items_center().child(name)
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let row = div().id(("row", row_ix));
        if self.is_deleted(row_ix) {
            row.bg(cx.theme().red.opacity(0.12)).text_color(cx.theme().muted_foreground).line_through()
        } else if (self.rows.len()..self.rows.len() + self.edits.inserted.len()).contains(&row_ix) {
            // New rows only: the table also renders filler rows past the end.
            row.bg(cx.theme().green.opacity(0.10))
        } else {
            row
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if let Some((_, _, input)) = self.editing.as_ref().filter(|(r, c, _)| (*r, *c) == (row_ix, col_ix)) {
            // Its own key context so Escape cancels this edit and not, say, the filter.
            return div()
                .key_context("CellEditor")
                .size_full()
                .px_2()
                .flex()
                .items_center()
                .child(Input::new(input).small().appearance(false).px_0())
                .into_any_element();
        }
        let cell = h_flex().size_full().px_2().py_1().overflow_hidden().whitespace_nowrap().text_ellipsis();
        let edited = row_ix < self.rows.len() && self.edits.updates.get(&row_ix).is_some_and(|c| c.contains_key(&col_ix));
        let cell = if edited { cell.bg(cx.theme().yellow.opacity(0.18)) } else { cell };
        let muted = cx.theme().muted_foreground;
        if edited || row_ix >= self.rows.len() {
            return match self.shown(row_ix, col_ix) {
                Some(EditValue::Text(text)) => cell.child(text.replace(['\n', '\r'], " ")),
                Some(EditValue::Null) => cell.text_color(muted).italic().child("NULL"),
                _ => cell.text_color(muted).italic().child("DEFAULT"),
            }
            .into_any_element();
        }
        let value = &self.rows[row_ix][col_ix];
        match value {
            Value::Null => cell.text_color(muted).italic().child("NULL"),
            Value::Int(_) | Value::Float(_) | Value::Decimal(_) => cell.justify_end().child(value.display()),
            // One line per cell: newlines would otherwise grow the row.
            _ => cell.child(value.display().replace(['\n', '\r'], " ")),
        }
        .into_any_element()
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
        if self.read_only_reason().is_none() {
            let grid = cx.entity().downgrade();
            let edit = move |cx: &mut App, f: &dyn Fn(&mut RowsDelegate)| {
                grid.update(cx, |state, cx| {
                    f(state.delegate_mut());
                    cx.notify();
                })
                .ok();
            };
            if let Some(col) = cell.filter(|&c| self.is_editable(c)) {
                if let Some(begin) = self.begin_edit.clone() {
                    menu = menu.item(PopupMenuItem::new("Edit Value").on_click(move |_, window, cx| begin(row_ix, col, window, cx)));
                }
                let (null, default) = (edit.clone(), edit.clone());
                menu = menu
                    .item(PopupMenuItem::new("Set to NULL").on_click(move |_, _, cx| null(cx, &|g| g.set_cell(row_ix, col, EditValue::Null))))
                    .item(PopupMenuItem::new("Set to DEFAULT").on_click(move |_, _, cx| {
                        default(cx, &|g| g.set_cell(row_ix, col, EditValue::Default))
                    }));
            }
            let label = if self.is_deleted(row_ix) { "Restore Row" } else { "Delete Row" };
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| edit(cx, &|g| g.toggle_delete(row_ix)))).separator();
        }
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
        self.next.is_some() && !self.loading_more && !self.reloading && self.load_more_error.is_none() && self.editing.is_none()
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
