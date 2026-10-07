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
use dbcore::results::{ResultSources, RowEdit};
use dbcore::{
    ColumnInfo, Connection, DatabaseKind, PageCursor, RowPage, RowQuery, SortKey, TableInfo,
    TableKind, Value,
};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
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


/// Rows of another table to open: those whose `columns` equal `values`. Empty `columns`: the
/// table's primary key (SQLite foreign keys can reference it implicitly).
#[derive(Clone, Debug)]
pub struct RelatedRows {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub values: Vec<Value>,
}

/// Opens related rows in a tab. Set by the table tab.
pub type OpenRelated = Rc<dyn Fn(RelatedRows, &mut Window, &mut App)>;

#[derive(Default)]
pub struct RowsDelegate {
    pub edits: PendingEdits,
    /// The cell being edited and its input.
    pub editing: Option<(usize, usize, Entity<InputState>)>,
    pub begin_edit: Option<BeginEdit>,
    /// What the columns are, from the tables they read (a table's own structure, or a script
    /// result's tables): foreign key links by column index, and, for script results, which cells
    /// can be edited. Loaded after the rows.
    pub links: Option<ResultSources>,
    pub open_related: Option<OpenRelated>,
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
    /// Where the rows were read (script results save their edits through it).
    connection: Option<Arc<Connection>>,
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
        self.connection = None;
        self.next = None;
        self.loading_more = false;
        self.load_more_error = None;
        self.reloading = false;
        self.reload_error = None;
        self.query = RowQuery::default();
        self.load_task = None;
    }

    /// Shows a script's result: all of it at once, no paging or sorting.
    pub fn show_result(&mut self, connection: Arc<Connection>, result: dbcore::QueryResult) {
        self.clear();
        self.kind = Some(connection.config().kind);
        self.connection = Some(connection);
        self.columns = result.columns;
        self.rows = result.rows;
        self.total = result.total_count;
        self.links = None;
    }

    /// Shows the first page of `table`, read with `query`; later pages load on scroll.
    pub fn show(&mut self, connection: Arc<Connection>, table: TableInfo, query: RowQuery, page: RowPage) {
        self.clear();
        self.query = query;
        self.kind = Some(connection.config().kind);
        self.columns = page.result.columns;
        self.rows = page.result.rows;
        self.total = page.result.total_count;
        self.next = page.next;
        self.source = Some((connection.clone(), table));
        self.connection = Some(connection);
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
        let Some((_, table)) = &self.source else {
            // Script results: cells of tables whose primary key is in the result.
            return match &self.links {
                Some(links) => links.summary_reason(),
                None => Some("Script results are read-only until the tables they come from are known.".into()),
            };
        };
        if table.kind == TableKind::View {
            return Some("Views are read-only.".into());
        }
        if !self.columns.iter().any(|c| c.is_primary_key) {
            return Some(format!("“{}” has no primary key, so its rows can’t be identified for editing.", table.name));
        }
        None
    }

    pub fn is_editable(&self, col: usize) -> bool {
        self.cell_read_only_reason(col).is_none()
    }

    /// Why `col`'s cells can't be edited (`None`: they can).
    pub fn cell_read_only_reason(&self, col: usize) -> Option<String> {
        if let Some(reason) = self.read_only_reason() {
            return Some(reason);
        }
        let column = self.columns.get(col)?;
        if is_binary(column) {
            return Some("Binary values can’t be edited here.".into());
        }
        match (&self.source, &self.links) {
            (None, Some(links)) => links.read_only_reason(col),
            _ => None,
        }
    }

    /// Rows can be deleted: a table's, or script results from one editable table.
    pub fn can_delete_rows(&self) -> bool {
        self.read_only_reason().is_none() && (self.source.is_some() || self.links.as_ref().is_some_and(|l| l.can_delete_rows()))
    }

    pub fn source(&self) -> Option<&(Arc<Connection>, TableInfo)> {
        self.source.as_ref()
    }

    pub fn connection(&self) -> Option<&Arc<Connection>> {
        self.connection.as_ref()
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

    /// Drops the pending edit of one cell (back to the loaded value).
    pub fn revert_cell(&mut self, row: usize, col: usize) {
        if let Some(cells) = self.edits.updates.get_mut(&row) {
            cells.remove(&col);
            if cells.is_empty() {
                self.edits.updates.remove(&row);
            }
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

    /// A script result's pending edits, row by row (the core groups them by table).
    pub fn result_edits(&self) -> Vec<RowEdit> {
        let mut edits: Vec<RowEdit> = self
            .edits
            .deleted
            .iter()
            .filter_map(|&row| Some(RowEdit { values: self.rows.get(row)?.clone(), set: Vec::new(), delete: true }))
            .collect();
        for (&row, cells) in &self.edits.updates {
            if self.edits.deleted.contains(&row) {
                continue;
            }
            let Some(values) = self.rows.get(row) else { continue };
            let set = cells.iter().map(|(&col, value)| (col, value.clone())).collect();
            edits.push(RowEdit { values: values.clone(), set, delete: false });
        }
        edits
    }

    /// After saving a script result's edits: the rows as saved (deleted ones dropped), without
    /// re-running the script, which may do more than select.
    pub fn apply_saved_edits(&mut self) {
        let edits = std::mem::take(&mut self.edits);
        for (&row, cells) in &edits.updates {
            let Some(values) = self.rows.get_mut(row) else { continue };
            for (&col, value) in cells {
                let Some(cell) = values.get_mut(col) else { continue };
                match value {
                    EditValue::Null => *cell = Value::Null,
                    EditValue::Default => {}
                    EditValue::Text(text) => *cell = typed_like(text, cell),
                }
            }
        }
        for &row in edits.deleted.iter().rev() {
            if row < self.rows.len() {
                self.rows.remove(row);
            }
        }
        self.editing = None;
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

    // MARK: foreign keys

    /// The loaded values of result columns `indexes` in `row`; `None` when one is NULL (it points
    /// nowhere), a column is missing, or the row is new.
    fn key_values(&self, row: usize, indexes: &[usize]) -> Option<Vec<Value>> {
        let values = self.rows.get(row)?;
        if indexes.is_empty() {
            return None;
        }
        indexes.iter().map(|&i| values.get(i).filter(|v| !v.is_null()).cloned()).collect()
    }

    /// The rows `row`'s foreign keys point at, with a menu label each.
    pub fn outgoing(&self, row: usize) -> Vec<(String, RelatedRows)> {
        let Some(links) = &self.links else { return Vec::new() };
        links
            .foreign_keys
            .iter()
            .filter_map(|fk| {
                let values = self.key_values(row, &fk.columns)?;
                let related = RelatedRows { schema: fk.schema.clone(), table: fk.table.clone(), columns: fk.target_columns.clone(), values };
                Some((fk.label.clone(), related))
            })
            .collect()
    }

    /// The link shown in a cell: the row its column's foreign key points at (single-column keys first).
    fn cell_link(&self, row: usize, col: usize) -> Option<RelatedRows> {
        let links = self.links.as_ref()?;
        let mut keys: Vec<_> = links.foreign_keys.iter().filter(|fk| fk.columns.contains(&col)).collect();
        keys.sort_by_key(|fk| fk.columns.len());
        let fk = keys.first()?;
        let values = self.key_values(row, &fk.columns)?;
        Some(RelatedRows { schema: fk.schema.clone(), table: fk.table.clone(), columns: fk.target_columns.clone(), values })
    }

    /// The rows in other tables whose foreign keys point at `row`, with a menu label each.
    pub fn incoming(&self, row: usize) -> Vec<(String, RelatedRows)> {
        let Some(links) = &self.links else { return Vec::new() };
        links
            .referenced_by
            .iter()
            .filter_map(|key| {
                let values = self.key_values(row, &key.values)?;
                let related = RelatedRows { schema: key.schema.clone(), table: key.table.clone(), columns: key.columns.clone(), values };
                Some((key.label.clone(), related))
            })
            .collect()
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
        let link = self.open_related.clone().zip(self.cell_link(row_ix, col_ix));
        if let Some((open, related)) = link {
            // A foreign key: the value, and an arrow to its row that shows on hover.
            let numeric = matches!(value, Value::Int(_) | Value::Float(_) | Value::Decimal(_));
            let text = div().flex_1().min_w_0().overflow_hidden().text_ellipsis().when(numeric, |d| d.text_right()).child(value.display());
            let tip = SharedString::from(format!("Open the {} row", related.table));
            let arrow = div()
                .id(SharedString::from(format!("fk-{row_ix}-{col_ix}")))
                .flex_shrink_0()
                .ml_1()
                .p_0p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(muted)
                .opacity(0.)
                .group_hover("fk-cell", |s| s.opacity(1.))
                .hover(|s| s.bg(cx.theme().muted).text_color(cx.theme().foreground))
                .child(Icon::new(IconName::ArrowRight).xsmall())
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                // Not a click on the cell: don't select it.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    open(related.clone(), window, cx);
                });
            return cell.group("fk-cell").child(text).child(arrow).into_any_element();
        }
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

/// The right-click menu for `row_ix` (and the cell in column `cell`): editing, the rows its
/// foreign keys lead to, and copying. Opened by the tabs (see `tabs::CellMenu`).
pub fn row_menu(
    grid: &Entity<TableState<RowsDelegate>>,
    row_ix: usize,
    cell: Option<usize>,
    mut menu: PopupMenu,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let rows = grid.read(cx).delegate();
    let editable = rows.read_only_reason().is_none();
    let can_delete = rows.can_delete_rows();
    let editable_cell = cell.filter(|&c| rows.is_editable(c));
    let begin_edit = rows.begin_edit.clone();
    let deleted = rows.is_deleted(row_ix);
    let open_related = rows.open_related.clone();
    let (outgoing, incoming) = (rows.outgoing(row_ix), rows.incoming(row_ix));
    let loaded = count(rows.rows.len() as u64);

    let state = grid.downgrade();
    let read = move |cx: &mut App, f: &dyn Fn(&RowsDelegate) -> Option<String>| {
        if let Some(text) = state.upgrade().and_then(|s| f(s.read(cx).delegate())) {
            copy(text, cx);
        }
    };
    if editable {
        let grid = grid.downgrade();
        let edit = move |cx: &mut App, f: &dyn Fn(&mut RowsDelegate)| {
            grid.update(cx, |state, cx| {
                f(state.delegate_mut());
                cx.notify();
            })
            .ok();
        };
        if let Some(col) = editable_cell {
            if let Some(begin) = begin_edit {
                menu = menu.item(PopupMenuItem::new("Edit Value").on_click(move |_, window, cx| begin(row_ix, col, window, cx)));
            }
            let (null, default) = (edit.clone(), edit.clone());
            menu = menu
                .item(PopupMenuItem::new("Set to NULL").on_click(move |_, _, cx| null(cx, &|g| g.set_cell(row_ix, col, EditValue::Null))))
                .item(PopupMenuItem::new("Set to DEFAULT").on_click(move |_, _, cx| {
                    default(cx, &|g| g.set_cell(row_ix, col, EditValue::Default))
                }));
        }
        if can_delete {
            let label = if deleted { "Restore Row" } else { "Delete Row" };
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| edit(cx, &|g| g.toggle_delete(row_ix))));
        }
        menu = menu.separator();
    }
    if let Some(open) = open_related {
        let related = !outgoing.is_empty() || !incoming.is_empty();
        for (label, target) in outgoing {
            let open = open.clone();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| open(target.clone(), window, cx)));
        }
        if !incoming.is_empty() {
            let referencing = PopupMenu::build(window, cx, move |mut sub, _, _| {
                for (label, target) in incoming.iter().cloned() {
                    let open = open.clone();
                    sub = sub.item(PopupMenuItem::new(label).on_click(move |_, window, cx| open(target.clone(), window, cx)));
                }
                sub
            });
            menu = menu.item(PopupMenuItem::submenu("Referenced By", referencing));
        }
        if related {
            menu = menu.separator();
        }
    }
    if let Some(col) = cell {
        let read = read.clone();
        menu = menu.item(PopupMenuItem::new("Copy Value").on_click(move |_, _, cx| read(cx, &|grid| grid.value_text(row_ix, col))));
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
    let row_formats = PopupMenu::build(window, cx, move |mut sub, _, _| {
        for (format, title) in COPY_FORMATS {
            let read = row_as.clone();
            sub = sub.item(PopupMenuItem::new(title).on_click(move |_, _, cx| read(cx, &|grid| Some(grid.format(&[row_ix], format, true)))));
        }
        sub
    });
    let all_formats = PopupMenu::build(window, cx, move |mut sub, _, _| {
        for (format, title) in COPY_FORMATS {
            let read = read.clone();
            sub = sub.item(PopupMenuItem::new(title).on_click(move |_, _, cx| {
                read(cx, &|grid| Some(grid.format(&(0..grid.rows.len()).collect::<Vec<_>>(), format, true)))
            }));
        }
        sub
    });
    menu.item(PopupMenuItem::submenu("Copy Row As", row_formats))
        .item(PopupMenuItem::submenu(format!("Copy All {loaded} Loaded Rows As"), all_formats))
}

/// Text saved into a cell, shown like the value it replaced (until the rows are read again).
fn typed_like(text: &str, original: &Value) -> Value {
    match original {
        Value::Int(_) => text.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(text.into())),
        Value::Float(_) => text.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(text.into())),
        Value::Decimal(_) if text.parse::<f64>().is_ok() => Value::Decimal(text.into()),
        Value::Bool(_) => match text.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => Value::Bool(true),
            "false" | "f" | "0" | "no" => Value::Bool(false),
            _ => Value::Text(text.into()),
        },
        _ => Value::Text(text.into()),
    }
}
