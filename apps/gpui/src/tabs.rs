//! What the right pane shows: a table (rows or structure), or a SQL script with its results.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbcore::edit::{EditStatement, EditValue};
use dbcore::export::CopyFormat;
use dbcore::{Connection, QueryResult, RowQuery, TableInfo, TableStructure};
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::{PAGE_SIZE, RowsDelegate, copy};
use crate::inspector::{self, Inspector};
use crate::sql_highlight;

/// Rows kept from one script result (like the macOS app); the rest are counted, not shown.
pub const SCRIPT_ROW_LIMIT: u32 = 10_000;

actions!(
    dbear,
    [RunScript, CancelScript, CopySelection, CopySelectionWithHeaders, ToggleInspector, CancelEdit, DeleteRow, SaveEdits]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // More specific than the editor's own ⌘↩ (insert a line), so running wins inside scripts.
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab > Input")),
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab")),
        KeyBinding::new("secondary-.", CancelScript, Some("ScriptTab")),
        KeyBinding::new("secondary-c", CopySelection, Some("DataTable")),
        KeyBinding::new("shift-secondary-c", CopySelectionWithHeaders, Some("DataTable")),
        KeyBinding::new("alt-secondary-i", ToggleInspector, None),
        KeyBinding::new("escape", CancelEdit, Some("CellEditor > Input")),
        KeyBinding::new("secondary-backspace", DeleteRow, Some("DataTable")),
        KeyBinding::new("secondary-s", SaveEdits, Some("TableTab")),
    ]);
}

/// A grid for rows: cells are selectable (for the inspector and copying a value).
fn new_grid(window: &mut Window, cx: &mut App) -> Entity<TableState<RowsDelegate>> {
    cx.new(|cx| TableState::new(RowsDelegate::default(), window, cx).cell_selectable(true).row_header(false))
}

/// ⌘C: the selected cell's value, or the selected row (TSV). ⇧⌘C adds the header line.
fn copy_selection(grid: &Entity<TableState<RowsDelegate>>, headers: bool, cx: &mut App) {
    let state = grid.read(cx);
    let rows = state.delegate();
    let text = match state.selection() {
        gpui_kit::component::table::TableSelection::Cell(row, col) if !headers => rows.value_text(row, col),
        selection => RowsDelegate::selected_row(selection).map(|row| rows.format(&[row], CopyFormat::Tsv, headers)),
    };
    if let Some(text) = text {
        copy(text, cx);
    }
}

/// The grid. Its focus ring (a shadow outside its edge, drawn after keyboard input, e.g. ↩ to
/// commit a cell) is clipped away: the selected cell already shows where focus is.
fn data_table(grid: &Entity<TableState<RowsDelegate>>) -> AnyElement {
    div().size_full().overflow_hidden().child(DataTable::new(grid).stripe(true).bordered(false)).into_any_element()
}

/// The grid, with the inspector beside it when it's shown.
fn grid_with_inspector(grid: AnyElement, inspector: &Entity<Inspector>, cx: &App) -> AnyElement {
    if !inspector::is_shown(cx) {
        return grid;
    }
    h_resizable("grid-inspector")
        .child(resizable_panel().child(grid))
        .child(resizable_panel().size(px(300.)).size_range(px(200.)..px(700.)).child(inspector.clone()))
        .into_any_element()
}

fn inspector_button(cx: &App) -> Button {
    Button::new("toggle-inspector")
        .ghost()
        .small()
        .icon(IconName::PanelRight)
        .selected(inspector::is_shown(cx))
        .tooltip("Show/Hide Inspector (⌥⌘I)")
        .on_click(|_, _, cx| inspector::toggle(cx))
}

enum Load {
    Loading,
    Loaded,
    Failed(String),
}

fn status_bar(text: String, cx: &App) -> impl IntoElement {
    h_flex()
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().status_bar)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn centered() -> Div {
    v_flex().size_full().items_center().justify_center()
}

fn message(title: &str, detail: String, cx: &App) -> Div {
    centered()
        .p_4()
        .gap_2()
        .child(div().font_semibold().child(title.to_string()))
        .child(div().text_sm().text_color(cx.theme().muted_foreground).text_center().child(detail))
}

pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{} {word}{}", count(n as u64), if n == 1 { "" } else { "s" })
}

fn rows_status(grid: &RowsDelegate) -> String {
    let shown = grid.rows.len();
    let rows = match grid.total {
        Some(total) if total as usize > shown => format!("{} of {}", count(shown as u64), plural(total as usize, "row")),
        _ => plural(shown, "row"),
    };
    format!("{rows} · {}", plural(grid.columns.len(), "column"))
}

// MARK: table

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Data,
    Structure,
}

enum Structure {
    NotLoaded,
    Loading,
    Loaded(TableStructure),
    Failed(String),
}

pub struct TableTab {
    pub table: TableInfo,
    /// Replaced by the next table opened with a single click; double-click keeps it.
    pub preview: bool,
    connection: Arc<Connection>,
    mode: Mode,
    rows: Load,
    grid: Entity<TableState<RowsDelegate>>,
    filter: Entity<InputState>,
    structure: Structure,
    inspector: Entity<Inspector>,
    /// Why the last edit attempt was refused (read-only table or column).
    edit_notice: Option<String>,
    edit_subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TableTab {
    pub fn new(connection: Arc<Connection>, table: TableInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let grid = new_grid(window, cx);
        let inspector = cx.new(|cx| Inspector::new(grid.clone(), cx));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows, e.g. status = 'paid' and total > 10"));
        // The grid's menu starts edits through the tab, which owns the input.
        let tab = cx.entity().downgrade();
        grid.update(cx, |state, _| {
            state.delegate_mut().begin_edit = Some(Rc::new(move |row, col, window, cx| {
                tab.update(cx, |tab, cx| tab.begin_edit(row, col, window, cx)).ok();
            }));
        });
        let subscriptions = vec![
            cx.observe(&grid, |this, grid, cx| {
                // A tab with unsaved edits isn't a preview anymore: the next table opens beside it.
                if !grid.read(cx).delegate().edits.is_empty() {
                    this.preview = false;
                }
                cx.notify();
            }),
            cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(row, col) = *event {
                    this.begin_edit(row, col, window, cx);
                }
            }),
            cx.observe_global::<inspector::ShowInspector>(|_, cx| cx.notify()),
            cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.apply_filter(cx);
                }
            }),
        ];
        let task = cx.spawn({
            let (connection, table) = (connection.clone(), table.clone());
            async move |this, cx| {
                let result = connection.fetch_page(table.clone(), RowQuery::default(), PAGE_SIZE, None).await;
                this.update(cx, |this, cx| {
                    this.rows = match result {
                        Ok(page) => {
                            this.grid.update(cx, |state, cx| {
                                state.delegate_mut().show(connection, table, page);
                                state.refresh(cx);
                            });
                            Load::Loaded
                        }
                        Err(e) => Load::Failed(e.to_string()),
                    };
                    cx.notify();
                })
                .ok();
            }
        });
        Self {
            table,
            preview: true,
            connection,
            mode: Mode::Data,
            rows: Load::Loading,
            grid,
            filter,
            structure: Structure::NotLoaded,
            inspector,
            edit_notice: None,
            edit_subscriptions: Vec::new(),
            _tasks: vec![task],
            _subscriptions: subscriptions,
        }
    }

    pub fn title(&self) -> String {
        self.table.name.clone()
    }

    pub fn has_unsaved_edits(&self, cx: &App) -> bool {
        !self.grid.read(cx).delegate().edits.is_empty()
    }

    // MARK: editing

    fn begin_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let (editable, reason, text) = {
            let grid = self.grid.read(cx).delegate();
            let reason = grid.read_only_reason().or_else(|| {
                (!grid.is_editable(col)).then(|| "Binary columns can’t be edited here.".to_string())
            });
            (reason.is_none() && !grid.is_deleted(row), reason, grid.edit_text(row, col))
        };
        if !editable {
            self.edit_notice = reason;
            cx.notify();
            return;
        }
        self.edit_notice = None;
        self.preview = false;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        self.edit_subscriptions = vec![cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_edit(window, cx),
            _ => {}
        })];
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            // Typing replaces the value; arrows keep it.
            s.select_all(window, cx);
        });
        self.grid.update(cx, |state, cx| {
            // The table's selection highlights the cell being edited (also when started from the menu).
            // Only when needed: selecting also scrolls the row to the middle.
            if state.selected_cell() != Some((row, col)) {
                state.set_selected_cell(row, col, cx);
            }
            state.delegate_mut().editing = Some((row, col, input));
            cx.notify();
        });
    }

    /// Keeps what was typed (when it differs from what the cell showed).
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col, input)) = self.grid.update(cx, |state, _| state.delegate_mut().editing.take()) else { return };
        self.edit_subscriptions.clear();
        let text = input.read(cx).value().to_string();
        self.grid.update(cx, |state, cx| {
            let grid = state.delegate_mut();
            if text != grid.edit_text(row, col) {
                grid.set_cell(row, col, EditValue::Text(text));
            }
            cx.notify();
        });
        self.focus_grid(window, cx);
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_subscriptions.clear();
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().editing = None;
            cx.notify();
        });
        self.focus_grid(window, cx);
    }

    fn focus_grid(&self, window: &mut Window, cx: &mut App) {
        let focus = self.grid.read(cx).focus_handle(cx);
        focus.focus(window, cx);
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (row, col) = self.grid.update(cx, |state, cx| {
            let grid = state.delegate_mut();
            let row = grid.add_row();
            // Start in the first column that isn't a generated key.
            let col = (0..grid.columns.len()).find(|&c| grid.is_editable(c) && !grid.columns[c].is_primary_key).unwrap_or(0);
            state.scroll_to_row(row, cx);
            cx.notify();
            (row, col)
        });
        self.begin_edit(row, col, window, cx);
    }

    fn delete_row(&mut self, _: &DeleteRow, _: &mut Window, cx: &mut Context<Self>) {
        self.grid.update(cx, |state, cx| {
            let Some(row) = RowsDelegate::selected_row(state.selection()) else { return };
            if state.delegate().read_only_reason().is_some() {
                return;
            }
            state.delegate_mut().toggle_delete(row);
            cx.notify();
        });
    }

    fn discard_edits(&mut self, cx: &mut Context<Self>) {
        self.edit_subscriptions.clear();
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().discard_edits();
            cx.notify();
        });
    }

    fn review(&mut self, _: &SaveEdits, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        if !self.has_unsaved_edits(cx) {
            return;
        }
        let review = cx.new(|cx| ReviewEdits::new(self.grid.clone(), cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let saving = review.read(cx).saving;
            let can_save = review.read(cx).statements.is_ok() && !saving;
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1())
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("save").primary().label(if saving { "Saving…" } else { "Save" }).disabled(!can_save).on_click({
                    let review = review.clone();
                    move |_, window, cx| review.update(cx, |r, cx| r.save(window, cx))
                }));
            dialog.title("Review Changes").w(px(760.)).child(review.clone()).footer(footer)
        });
    }

    /// Reads the rows again with the typed `WHERE` filter (empty: no filter).
    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let text = self.filter.read(cx).value().trim().to_string();
        let filter = (!text.is_empty()).then_some(text);
        self.grid.update(cx, |state, cx| {
            if state.delegate().query.filter == filter {
                return;
            }
            let mut query = state.delegate().query.clone();
            query.filter = filter;
            state.delegate_mut().reload(query, cx);
        });
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |s, cx| s.set_value("", window, cx));
        self.apply_filter(cx);
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        if mode == Mode::Structure && matches!(self.structure, Structure::NotLoaded | Structure::Failed(_)) {
            self.structure = Structure::Loading;
            let (connection, table) = (self.connection.clone(), self.table.clone());
            self._tasks.push(cx.spawn(async move |this, cx| {
                let result = connection.describe_table(table).await;
                this.update(cx, |this, cx| {
                    this.structure = match result {
                        Ok(structure) => Structure::Loaded(structure),
                        Err(e) => Structure::Failed(e.to_string()),
                    };
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn render_data(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.grid.read(cx).delegate();
        let applied = grid.query.filter.clone();
        let typed = self.filter.read(cx).value().trim().to_string();
        let pending = typed != applied.clone().unwrap_or_default();
        let error = grid.reload_error.clone().or_else(|| self.edit_notice.clone());
        let edits = (!grid.edits.is_empty()).then(|| grid.edits.summary());
        let _ = window;
        let filter_bar = h_flex()
            .px_2()
            .py_1p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_xs().font_semibold().text_color(cx.theme().muted_foreground).child("WHERE"))
            .child(div().flex_1().child(Input::new(&self.filter).small()))
            .when(pending, |bar| bar.child(div().text_xs().text_color(cx.theme().muted_foreground).child("↩ to apply")))
            .when(applied.is_some(), |bar| {
                bar.child(
                    Button::new("clear-filter")
                        .ghost()
                        .xsmall()
                        .label("Show All Rows")
                        .on_click(cx.listener(|this, _, window, cx| this.clear_filter(window, cx))),
                )
            });
        let body = match &self.rows {
            Load::Loading => centered().child(Spinner::new()).into_any_element(),
            Load::Failed(e) => message("Couldn’t Load Rows", e.clone(), cx).into_any_element(),
            Load::Loaded => {
                let table = data_table(&self.grid);
                grid_with_inspector(table, &self.inspector, cx)
            }
        };
        let edit_bar = edits.map(|summary| {
            h_flex()
                .px_3()
                .py_1p5()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().yellow.opacity(0.10))
                .child(div().flex_1().text_sm().child(format!("Unsaved changes: {summary}")))
                .child(Button::new("discard").ghost().small().label("Discard").on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))))
                .child(
                    Button::new("review")
                        .primary()
                        .small()
                        .label("Review & Save… (⌘S)")
                        .on_click(cx.listener(|this, _, window, cx| this.review(&SaveEdits, window, cx))),
                )
        });
        v_flex()
            .size_full()
            .child(filter_bar)
            .children(error.map(|e| div().px_3().py_1().text_xs().text_color(cx.theme().red).child(e)))
            .child(div().flex_1().min_h_0().child(body))
            .children(edit_bar)
            .into_any_element()
    }

    fn render_structure(&self, cx: &App) -> AnyElement {
        let structure = match &self.structure {
            Structure::NotLoaded | Structure::Loading => return centered().child(Spinner::new()).into_any_element(),
            Structure::Failed(e) => return message("Couldn’t Load the Structure", e.clone(), cx).into_any_element(),
            Structure::Loaded(structure) => structure,
        };
        let yes = |b: bool| if b { "yes" } else { "" }.to_string();
        let list = |v: &[String]| v.join(", ");
        let mut sections = v_flex().gap_6().p_4();
        sections = sections.child(section(
            "Columns",
            &["Name", "Type", "Nullable", "Default", "Comment"],
            structure
                .columns
                .iter()
                .map(|c| {
                    let name = if c.is_primary_key { format!("🔑 {}", c.name) } else { c.name.clone() };
                    vec![name, c.type_name.clone(), yes(c.is_nullable), c.default_value.clone().unwrap_or_default(), c.comment.clone().unwrap_or_default()]
                })
                .collect(),
            cx,
        ));
        if !structure.indexes.is_empty() {
            sections = sections.child(section(
                "Indexes",
                &["Name", "Columns", "Unique", "Primary"],
                structure.indexes.iter().map(|i| vec![i.name.clone(), list(&i.columns), yes(i.is_unique), yes(i.is_primary)]).collect(),
                cx,
            ));
        }
        if !structure.foreign_keys.is_empty() {
            sections = sections.child(section(
                "Foreign Keys",
                &["Name", "Columns", "References", "On Update", "On Delete"],
                structure
                    .foreign_keys
                    .iter()
                    .map(|f| {
                        let target = format!("{}.{}({})", f.referenced_schema, f.referenced_table, list(&f.referenced_columns));
                        vec![f.name.clone(), list(&f.columns), target, f.on_update.clone(), f.on_delete.clone()]
                    })
                    .collect(),
                cx,
            ));
        }
        if !structure.referenced_by.is_empty() {
            sections = sections.child(section(
                "Referenced By",
                &["Table", "Columns", "Points At", "Name"],
                structure
                    .referenced_by
                    .iter()
                    .map(|r| vec![format!("{}.{}", r.schema, r.table), list(&r.columns), list(&r.referenced_columns), r.name.clone()])
                    .collect(),
                cx,
            ));
        }
        if let Some(ddl) = &structure.ddl {
            sections = sections.child(
                v_flex()
                    .gap_2()
                    .child(h_flex().gap_2().child(div().font_semibold().child("DDL")).child(div().flex_1()).child({
                        let ddl = ddl.clone();
                        Button::new("copy-ddl").ghost().xsmall().icon(IconName::Copy).label("Copy").on_click(move |_, _, cx| copy(ddl.clone(), cx))
                    }))
                    .child(
                        div()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().border)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .child(SelectableText::new("ddl", ddl.clone())),
                    ),
            );
        }
        div().id("structure").size_full().overflow_y_scrollbar().child(sections).into_any_element()
    }
}

/// A small read-only table for the structure view.
fn section(title: &str, headers: &[&str], rows: Vec<Vec<String>>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let cell = |text: String| div().flex_1().min_w_0().px_2().py_1().truncate().child(text);
    let mut table = v_flex().rounded_md().border_1().border_color(theme.border).overflow_hidden().child(
        h_flex()
            .bg(theme.table_head)
            .text_xs()
            .font_semibold()
            .text_color(theme.muted_foreground)
            .children(headers.iter().map(|h| cell(h.to_string()))),
    );
    for (i, row) in rows.into_iter().enumerate() {
        table = table.child(
            h_flex()
                .text_sm()
                .border_t_1()
                .border_color(theme.border)
                .when(i % 2 == 1, |r| r.bg(theme.table_even))
                .children(row.into_iter().map(cell)),
        );
    }
    v_flex().gap_2().child(div().font_semibold().child(title.to_string())).child(table)
}

impl Render for TableTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.mode {
            Mode::Data => self.render_data(window, cx),
            Mode::Structure => self.render_structure(cx),
        };
        let status = match (&self.rows, self.mode) {
            (_, Mode::Structure) => match &self.structure {
                Structure::Loaded(s) => format!(
                    "{} · {} · {}",
                    plural(s.columns.len(), "column"),
                    plural(s.indexes.len(), "index"),
                    plural(s.foreign_keys.len(), "foreign key")
                )
                .replace("indexs", "indexes"),
                _ => String::new(),
            },
            (Load::Loaded, Mode::Data) => {
                let grid = self.grid.read(cx).delegate();
                let mut status = rows_status(grid);
                if grid.reloading {
                    status = "Loading…".into();
                } else if grid.loading_more {
                    status.push_str(" · Loading more…");
                } else if let Some(error) = &grid.load_more_error {
                    status.push_str(&format!(" · Couldn’t load more: {error}"));
                }
                if let Some(key) = grid.query.sort.first() {
                    status.push_str(&format!(" · sorted by {}{}", key.column, if key.descending { " ↓" } else { " ↑" }));
                }
                status
            }
            (Load::Loading, _) => "Loading…".into(),
            (Load::Failed(_), _) => String::new(),
        };
        let mode = self.mode;
        let editable = matches!(self.rows, Load::Loaded) && self.grid.read(cx).delegate().read_only_reason().is_none();
        let mode_button = |id: &'static str, label: &'static str, m: Mode, cx: &mut Context<Self>| {
            Button::new(id).ghost().small().label(label).selected(mode == m).on_click(cx.listener(move |this, _, _, cx| this.set_mode(m, cx)))
        };
        v_flex()
            .key_context("TableTab")
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| copy_selection(&this.grid, false, cx)))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| copy_selection(&this.grid, true, cx)))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(Self::delete_row))
            .on_action(cx.listener(Self::review))
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().font_semibold().child(self.table.name.clone()))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(self.table.schema.clone()))
                    .child(div().flex_1())
                    .when(mode == Mode::Data && editable, |h| {
                        h.child(
                            Button::new("add-row")
                                .ghost()
                                .small()
                                .icon(IconName::Plus)
                                .label("Row")
                                .tooltip("Add a row")
                                .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                        )
                    })
                    .child(mode_button("mode-data", "Data", Mode::Data, cx))
                    .child(mode_button("mode-structure", "Structure", Mode::Structure, cx))
                    .when(mode == Mode::Data, |h| h.child(inspector_button(cx))),
            )
            .child(div().flex_1().min_h_0().child(body))
            .child(status_bar(status, cx))
    }
}

// MARK: review

/// The SQL that saving will run, and the Save itself (one transaction in the core).
pub struct ReviewEdits {
    grid: Entity<TableState<RowsDelegate>>,
    statements: Result<Vec<EditStatement>, String>,
    error: Option<String>,
    saving: bool,
    task: Option<Task<()>>,
}

impl ReviewEdits {
    fn new(grid: Entity<TableState<RowsDelegate>>, cx: &mut Context<Self>) -> Self {
        let statements = {
            let rows = grid.read(cx).delegate();
            match rows.source() {
                Some((connection, table)) => {
                    connection.preview_changes(table, &rows.columns, &rows.changes()).map_err(|e| e.to_string())
                }
                None => Err("Nothing to save.".into()),
            }
        };
        Self { grid, statements, error: None, saving: false, task: None }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let (source, columns, changes) = {
            let rows = self.grid.read(cx).delegate();
            (rows.source().cloned(), rows.columns.clone(), rows.changes())
        };
        let Some((connection, table)) = source else { return };
        self.saving = true;
        self.error = None;
        let grid = self.grid.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = connection.apply_changes(table, columns, changes).await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        grid.update(cx, |state, cx| {
                            state.delegate_mut().discard_edits();
                            state.delegate_mut().refresh_rows(cx);
                        });
                        window.close_dialog(cx);
                    }
                    // Nothing was saved: the edits stay so they can be fixed.
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for ReviewEdits {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let summary = self.grid.read(cx).delegate().edits.summary();
        let body = match &self.statements {
            Ok(statements) => {
                let sql = statements.iter().map(|s| format!("{};", s.sql.trim_end().trim_end_matches(';'))).collect::<Vec<_>>().join("\n");
                div()
                    .id("review-sql")
                    .max_h(px(360.))
                    .overflow_y_scrollbar()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .child(SelectableText::new("review-sql-text", sql))
                    .into_any_element()
            }
            Err(e) => div().text_color(theme.red).child(e.clone()).into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                "{summary}. These statements run in one transaction: if any fails, none are saved."
            )))
            .child(body)
            .children(self.error.clone().map(|e| div().text_sm().text_color(theme.red).child(e)))
    }
}

// MARK: script

enum Outcome {
    Idle,
    Running(Instant),
    Rows { took: Duration, truncated: bool },
    Affected { rows: Option<u64>, took: Duration },
    Failed(String),
    Cancelled,
}

pub struct ScriptTab {
    /// "SQL 1", "SQL 2"…
    pub name: String,
    /// The database it runs in (shown in the header).
    pub target: String,
    connection: Arc<Connection>,
    editor: Entity<EditorState>,
    grid: Entity<TableState<RowsDelegate>>,
    inspector: Entity<Inspector>,
    outcome: Outcome,
    run_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ScriptTab {
    pub fn new(
        connection: Arc<Connection>,
        name: String,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).language(sql_highlight::LANGUAGE).line_number(true);
            state.set_highlighter_factory(sql_highlight::factory(), cx);
            state
        });
        let grid = new_grid(window, cx);
        let inspector = cx.new(|cx| Inspector::new(grid.clone(), cx));
        let subscriptions = vec![cx.observe_global::<inspector::ShowInspector>(|_, cx| cx.notify())];
        Self {
            name,
            target,
            connection,
            editor,
            grid,
            inspector,
            outcome: Outcome::Idle,
            run_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    /// The selection when there is one, else the whole script.
    fn sql(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let selected = editor.selected_text().to_string();
        if selected.trim().is_empty() { editor.value().to_string() } else { selected }
    }

    fn run(&mut self, _: &RunScript, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.outcome, Outcome::Running(_)) {
            return;
        }
        let sql = self.sql(cx);
        if sql.trim().is_empty() {
            return;
        }
        let started = Instant::now();
        self.outcome = Outcome::Running(started);
        let connection = self.connection.clone();
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.execute_limited(sql, Some(SCRIPT_ROW_LIMIT)).await;
            this.update(cx, |this, cx| {
                this.show(result, started.elapsed(), cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn show(&mut self, result: dbcore::Result<QueryResult>, took: Duration, cx: &mut Context<Self>) {
        self.outcome = match result {
            Ok(result) if result.columns.is_empty() => Outcome::Affected { rows: result.rows_affected, took },
            Ok(result) => {
                let truncated = result.truncated;
                let kind = self.connection.config().kind;
                self.grid.update(cx, |state, cx| {
                    state.delegate_mut().show_result(kind, result);
                    state.refresh(cx);
                });
                Outcome::Rows { took, truncated }
            }
            Err(dbcore::Error::Cancelled) => Outcome::Cancelled,
            Err(e) => Outcome::Failed(e.to_string()),
        };
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelScript, _: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.outcome, Outcome::Running(_)) {
            return;
        }
        let connection = self.connection.clone();
        cx.spawn(async move |_, _| connection.cancel().await).detach();
    }
}

fn took(duration: Duration) -> String {
    if duration.as_millis() < 1000 {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

impl Render for ScriptTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = matches!(self.outcome, Outcome::Running(_));
        let run = if running {
            Button::new("stop").small().icon(IconName::Square).label("Stop").on_click(cx.listener(|this, _, window, cx| this.cancel(&CancelScript, window, cx)))
        } else {
            Button::new("run").small().primary().icon(IconName::Play).label("Run").on_click(cx.listener(|this, _, window, cx| this.run(&RunScript, window, cx)))
        };
        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().font_semibold().child(self.name.clone()))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(self.target.clone()))
            .child(div().flex_1())
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(if running { "⌘. to stop" } else { "⌘↩ runs the selection or the script" }))
            .child(inspector_button(cx))
            .child(run);

        let results = match &self.outcome {
            Outcome::Idle => centered().text_color(cx.theme().muted_foreground).child("Run a query to see its results").into_any_element(),
            Outcome::Running(_) => centered().child(Spinner::new()).into_any_element(),
            Outcome::Rows { .. } => {
                grid_with_inspector(data_table(&self.grid), &self.inspector, cx)
            }
            Outcome::Affected { rows, .. } => centered()
                .text_lg()
                .child(match rows {
                    Some(n) => format!("{} affected", plural(*n as usize, "row")),
                    None => "Done".into(),
                })
                .into_any_element(),
            Outcome::Failed(e) => message("Query Failed", e.clone(), cx).into_any_element(),
            Outcome::Cancelled => centered().text_color(cx.theme().muted_foreground).child("Query cancelled").into_any_element(),
        };
        let status = match &self.outcome {
            Outcome::Rows { took: t, truncated } => {
                let mut status = format!("{} · {}", rows_status(self.grid.read(cx).delegate()), took(*t));
                if *truncated {
                    status.push_str(&format!(" · first {} rows shown", count(u64::from(SCRIPT_ROW_LIMIT))));
                }
                status
            }
            Outcome::Affected { took: t, .. } => took(*t),
            Outcome::Running(started) => format!("Running… {}", took(started.elapsed())),
            _ => String::new(),
        };

        v_flex()
            .key_context("ScriptTab")
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| copy_selection(&this.grid, false, cx)))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| copy_selection(&this.grid, true, cx)))
            .size_full()
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("script-split")
                        .child(resizable_panel().size(px(260.)).size_range(px(80.)..px(2000.)).child(
                            Editor::new(&self.editor).size_full().border_0().font_family(cx.theme().mono_font_family.clone()).text_sm(),
                        ))
                        .child(resizable_panel().child(results)),
                ),
            )
            .child(status_bar(status, cx))
    }
}
