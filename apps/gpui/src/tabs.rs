//! What the right pane shows: a table (rows or structure), or a SQL script with its results.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbcore::edit::{EditStatement, EditValue};
use dbcore::export::CopyFormat;
use dbcore::{Connection, QueryResult, RowQuery, TableInfo, TableStructure};
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarMode};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::{PAGE_SIZE, RelatedRows, RowsDelegate, copy, row_menu};
use dbcore::results::{ResultSources, TableChanges};
use crate::inspector::{self, Inspector};
use crate::sql_complete::{SharedCatalog, SqlCompletion};
use crate::highlight;
use crate::keys;
use dbcore::complete::Catalog;
use dbcore::dialect::Dialect;
use dbcore::state::{NewHistoryEntry, StateStore};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

/// What a tab asks of the workspace.
pub enum TabEvent {
    /// Open rows of another table (a foreign key, either way) in a tab of their own.
    OpenRelated(RelatedRows),
    /// A script ran DDL: the tables column should list the tables again.
    SchemaMayHaveChanged,
    /// Show a script's results in a tab of their own.
    OpenResults(OpenResults),
}

/// Rows kept from one script result (like the macOS app); the rest are counted, not shown.
pub const SCRIPT_ROW_LIMIT: u32 = 10_000;

actions!(
    dbear,
    [
        RunScript,
        RunInNewTab,
        CancelScript,
        CopySelection,
        CopySelectionWithHeaders,
        ToggleInspector,
        CancelEdit,
        DeleteRow,
        SaveEdits,
        StartEdit,
        NextCell,
        PreviousCell,
        ZoomIn,
        ZoomOut,
        ResetZoom
    ]
);

/// The script editor's text size in points, the same for every script (⌘+ / ⌘- / ⌘0, like the
/// macOS app). Saved by the workspace.
pub struct EditorFontSize(pub f32);

impl Global for EditorFontSize {}

impl EditorFontSize {
    pub const DEFAULT: f32 = 14.;
    pub const RANGE: std::ops::RangeInclusive<f32> = 8.0..=40.0;

    pub fn get(cx: &App) -> f32 {
        cx.try_global::<Self>().map_or(Self::DEFAULT, |size| size.0)
    }

    fn set(size: f32, cx: &mut App) {
        cx.set_global(Self(size.clamp(*Self::RANGE.start(), *Self::RANGE.end())));
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // More specific than the editor's own ⌘↩ (insert a line), so running wins inside scripts.
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab > Input")),
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab")),
        KeyBinding::new("shift-secondary-enter", RunInNewTab, Some("ScriptTab > Input")),
        KeyBinding::new("shift-secondary-enter", RunInNewTab, Some("ScriptTab")),
        KeyBinding::new("secondary-.", CancelScript, Some("ScriptTab")),
        KeyBinding::new("secondary-enter", RunScript, Some("ResultTab")),
        KeyBinding::new("secondary-.", CancelScript, Some("ResultTab")),
        KeyBinding::new("secondary-c", CopySelection, Some("DataTable")),
        KeyBinding::new("shift-secondary-c", CopySelectionWithHeaders, Some("DataTable")),
        KeyBinding::new("alt-secondary-i", ToggleInspector, None),
        KeyBinding::new("escape", CancelEdit, Some("CellEditor > Input")),
        KeyBinding::new("enter", StartEdit, Some("DataTable")),
        KeyBinding::new("tab", NextCell, Some("CellEditor > Input")),
        KeyBinding::new("shift-tab", PreviousCell, Some("CellEditor > Input")),
        KeyBinding::new("secondary-backspace", DeleteRow, Some("DataTable")),
        KeyBinding::new("secondary-s", SaveEdits, Some("TableTab")),
        KeyBinding::new("secondary-s", SaveEdits, Some("Results")),
        KeyBinding::new("secondary-=", ZoomIn, Some("ScriptTab")),
        KeyBinding::new("secondary-+", ZoomIn, Some("ScriptTab")),
        KeyBinding::new("secondary--", ZoomOut, Some("ScriptTab")),
        KeyBinding::new("secondary-0", ResetZoom, Some("ScriptTab")),
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
///
/// `settling`: drawn fully transparent. A new table measures its height while it draws, and only
/// adds the striped filler rows below the data on the frame after; drawing that first frame
/// invisibly avoids a flash of empty space (see `Settle`).
///
/// The horizontal scrollbar is always shown when the columns don't fit (so it's clear there's more
/// to the right); the vertical one follows the system setting like every other list.
fn data_table(grid: &Entity<TableState<RowsDelegate>>, settling: bool, menu: &CellMenu, cx: &App) -> AnyElement {
    let horizontal = grid.read(cx).horizontal_scroll_handle.clone();
    let pointer = menu.pointer.clone();
    div()
        .id("grid")
        .relative()
        // Before the cell sees it: where a right-click's menu opens.
        .capture_any_mouse_down(move |event, _, _| pointer.set(event.position))
        .size_full()
        .overflow_hidden()
        .when(settling, |d| d.opacity(0.))
        .child(DataTable::new(grid).stripe(true).bordered(false).scrollbar_visible(true, false))
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(Scrollbar::width())
                .child(Scrollbar::horizontal(&horizontal).viewport_from_layout().mode(ScrollbarMode::Always)),
        )
        .into_any_element()
}

/// The grid's right-click menu. gpui-component's table opens its own menu only for a right-click
/// on a row; with cells selectable, the cell takes the click, so the tab opens this one instead.
#[derive(Default)]
struct CellMenu {
    /// Where the mouse last went down on the grid: the menu opens there.
    pointer: Rc<Cell<Point<Pixels>>>,
    open: Option<(Entity<PopupMenu>, Point<Pixels>)>,
    _dismiss: Option<Subscription>,
}

impl CellMenu {
    fn open<T: 'static>(
        &mut self,
        grid: &Entity<TableState<RowsDelegate>>,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<T>,
        get: fn(&mut T) -> &mut CellMenu,
    ) {
        let grid = grid.clone();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| row_menu(&grid, row, Some(col), menu, window, cx));
        self._dismiss = Some(cx.subscribe_in(&menu, window, move |this, _, _: &DismissEvent, _, cx| {
            get(this).open = None;
            cx.notify();
        }));
        menu.read(cx).focus_handle(cx).focus(window, cx);
        self.open = Some((menu, self.pointer.get()));
        cx.notify();
    }

    fn render(&self) -> Option<AnyElement> {
        let (menu, position) = self.open.clone()?;
        Some(
            deferred(anchored().position(position).snap_to_window_with_margin(px(8.)).child(menu))
                .with_priority(1)
                .into_any_element(),
        )
    }
}

/// Hides a grid for the frame it first appears in (see `data_table`).
#[derive(Default)]
struct Settle(bool);

impl Settle {
    fn start(&mut self) {
        self.0 = true;
    }

    /// Called from `render`: shows the grid again on the next frame.
    fn tick<T: 'static>(&self, window: &mut Window, cx: &mut Context<T>, get: fn(&mut T) -> &mut Settle) {
        if self.0 {
            cx.on_next_frame(window, move |this, _, cx| {
                get(this).0 = false;
                cx.notify();
            });
        }
    }
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
        .tooltip(format!("Show/Hide Inspector ({})", keys::shortcut("alt-secondary-i")))
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
    editor: GridEditor,
    settle: Settle,
    menu: CellMenu,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TableTab {
    /// `filter`: a `WHERE` condition to start with (rows opened through a foreign key).
    pub fn new(
        connection: Arc<Connection>,
        table: TableInfo,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = new_grid(window, cx);
        let inspector = cx.new(|cx| Inspector::new(grid.clone(), window, cx));
        let typed = filter.clone().unwrap_or_default();
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter rows, e.g. status = 'paid' and total > 10").default_value(typed)
        });
        let query = RowQuery { filter, ..RowQuery::default() };
        // The grid's menu starts edits and opens related rows through the tab.
        let tab = cx.entity().downgrade();
        grid.update(cx, |state, _| {
            let rows = state.delegate_mut();
            rows.query = query.clone();
            let edit_tab = tab.clone();
            rows.begin_edit = Some(Rc::new(move |row, col, window, cx| {
                edit_tab.update(cx, |tab, cx| tab.begin_edit(row, col, window, cx)).ok();
            }));
            rows.open_related = Some(Rc::new(move |related, _, cx| {
                tab.update(cx, |_, cx| cx.emit(TabEvent::OpenRelated(related))).ok();
            }));
        });
        let filter = filter_input;
        let subscriptions = vec![
            cx.observe(&grid, |this, grid, cx| {
                // A tab with unsaved edits isn't a preview anymore: the next table opens beside it.
                if !grid.read(cx).delegate().edits.is_empty() {
                    this.preview = false;
                }
                cx.notify();
            }),
            cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| match *event {
                TableEvent::DoubleClickedCell(row, col) => this.begin_edit(row, col, window, cx),
                TableEvent::RightClickedCell(row, col) => {
                    this.menu.open(&this.grid, row, col, window, cx, |this: &mut Self| &mut this.menu)
                }
                _ => {}
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
                let result = connection.fetch_page(table.clone(), query.clone(), PAGE_SIZE, None).await;
                this.update(cx, |this, cx| {
                    this.rows = match result {
                        Ok(page) => {
                            this.grid.update(cx, |state, cx| {
                                state.delegate_mut().show(connection, table, query, page);
                                state.refresh(cx);
                            });
                            this.settle.start();
                            // The foreign keys make cells into links; the structure view reuses it.
                            this.load_structure(cx);
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
            grid: grid.clone(),
            filter,
            structure: Structure::NotLoaded,
            inspector,
            editor: GridEditor::new(grid.clone()),
            settle: Settle::default(),
            menu: CellMenu::default(),
            _tasks: vec![task],
            _subscriptions: subscriptions,
        }
    }

    pub fn title(&self) -> String {
        self.table.name.clone()
    }

    /// The `WHERE` filter the rows are read with.
    pub fn applied_filter(&self, cx: &App) -> Option<String> {
        self.grid.read(cx).delegate().query.filter.clone()
    }

    pub fn has_unsaved_edits(&self, cx: &App) -> bool {
        !self.grid.read(cx).delegate().edits.is_empty()
    }

    // MARK: editing

    fn begin_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.begin(row, col, window, cx, |this: &mut Self| &mut this.editor) {
            self.preview = false;
        }
    }

    fn focus_grid(&self, window: &mut Window, cx: &mut App) {
        self.editor.focus_grid(window, cx);
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

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = mode;
        if mode == Mode::Data {
            self.focus_grid(window, cx);
        }
        if mode == Mode::Structure && matches!(self.structure, Structure::Failed(_)) {
            self.structure = Structure::NotLoaded;
        }
        if mode == Mode::Structure {
            self.load_structure(cx);
        }
        cx.notify();
    }

    /// Describes the table once: for the structure view, and the grid's foreign key links.
    fn load_structure(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.structure, Structure::NotLoaded) {
            return;
        }
        self.structure = Structure::Loading;
        let (connection, table) = (self.connection.clone(), self.table.clone());
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = connection.describe_table(table).await;
            this.update(cx, |this, cx| {
                this.structure = match result {
                    Ok(structure) => {
                        let table = this.table.clone();
                        this.grid.update(cx, |state, cx| {
                            let rows = state.delegate_mut();
                            rows.links = Some(ResultSources::for_table(&table, &rows.columns, &structure));
                            cx.notify();
                        });
                        Structure::Loaded(structure)
                    }
                    Err(e) => Structure::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_data(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.grid.read(cx).delegate();
        let applied = grid.query.filter.clone();
        let typed = self.filter.read(cx).value().trim().to_string();
        let pending = typed != applied.clone().unwrap_or_default();
        let error = grid.reload_error.clone().or_else(|| self.editor.notice.clone());
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
                let table = data_table(&self.grid, self.settle.0, &self.menu, cx);
                grid_with_inspector(table, &self.inspector, cx)
            }
        };
        let edit_bar = edit_bar(&self.grid, cx, |this: &mut Self| &mut this.editor);
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

impl EventEmitter<TabEvent> for TableTab {}

impl Render for TableTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settle.tick(window, cx, |this: &mut Self| &mut this.settle);
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
            Button::new(id).ghost().small().label(label).selected(mode == m).on_click(cx.listener(move |this, _, window, cx| this.set_mode(m, window, cx)))
        };
        v_flex()
            .key_context("TableTab")
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| copy_selection(&this.grid, false, cx)))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| copy_selection(&this.grid, true, cx)))
            .on_action(cx.listener(|this, _: &CancelEdit, window, cx| this.editor.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &StartEdit, window, cx| {
                if this.editor.start(window, cx, |this: &mut Self| &mut this.editor) {
                    this.preview = false;
                }
            }))
            .on_action(cx.listener(|this, _: &NextCell, window, cx| this.editor.step(true, window, cx, |this: &mut Self| &mut this.editor)))
            .on_action(cx.listener(|this, _: &PreviousCell, window, cx| this.editor.step(false, window, cx, |this: &mut Self| &mut this.editor)))
            .on_action(cx.listener(|this, _: &DeleteRow, _, cx| this.editor.delete_selected_row(cx)))
            .on_action(cx.listener(|this, _: &SaveEdits, window, cx| this.editor.review(window, cx)))
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
            .children(self.menu.render())
    }
}

// MARK: cell editing

/// Inline cell editing on a grid: table tabs and script results share it. The owner forwards the
/// grid's double-clicks and the editing actions, and finds its editor again with `get`.
struct GridEditor {
    grid: Entity<TableState<RowsDelegate>>,
    /// Why the last edit attempt was refused (read-only table or column).
    notice: Option<String>,
    subscriptions: Vec<Subscription>,
}

impl GridEditor {
    fn new(grid: Entity<TableState<RowsDelegate>>) -> Self {
        Self { grid, notice: None, subscriptions: Vec::new() }
    }

    /// Starts editing a cell. `false` (with `notice` saying why) when it can't be edited.
    fn begin<T: 'static>(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<T>, get: fn(&mut T) -> &mut GridEditor) -> bool {
        self.commit(window, cx);
        let (reason, deleted, text) = {
            let grid = self.grid.read(cx).delegate();
            (grid.cell_read_only_reason(col), grid.is_deleted(row), grid.edit_text(row, col))
        };
        if reason.is_some() || deleted {
            self.notice = reason;
            cx.notify();
            return false;
        }
        self.notice = None;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        self.subscriptions = vec![cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => get(this).commit(window, cx),
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
        true
    }

    /// Keeps what was typed (when it differs from what the cell showed).
    fn commit<T: 'static>(&mut self, window: &mut Window, cx: &mut Context<T>) {
        let Some((row, col, input)) = self.grid.update(cx, |state, _| state.delegate_mut().editing.take()) else { return };
        self.subscriptions.clear();
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

    /// ↩ on the grid: edit the selected cell (or the first editable cell of the selected row).
    fn start<T: 'static>(&mut self, window: &mut Window, cx: &mut Context<T>, get: fn(&mut T) -> &mut GridEditor) -> bool {
        let target = {
            let state = self.grid.read(cx);
            let grid = state.delegate();
            match state.selection() {
                gpui_kit::component::table::TableSelection::Cell(row, col) => Some((row, col)),
                gpui_kit::component::table::TableSelection::Row(row) => {
                    (0..grid.columns.len()).find(|&c| grid.is_editable(c)).map(|col| (row, col))
                }
                _ => None,
            }
        };
        target.is_some_and(|(row, col)| self.begin(row, col, window, cx, get))
    }

    /// Tab / ⇧Tab while editing: keep the value and edit the next (previous) editable cell,
    /// continuing on the next (previous) row at the end of one.
    fn step<T: 'static>(&mut self, forward: bool, window: &mut Window, cx: &mut Context<T>, get: fn(&mut T) -> &mut GridEditor) {
        let Some((row, col)) = self.grid.read(cx).delegate().editing.as_ref().map(|(r, c, _)| (*r, *c)) else { return };
        let next = {
            let grid = self.grid.read(cx).delegate();
            let (columns, rows) = (grid.columns.len(), grid.rows.len() + grid.edits.inserted.len());
            let total = columns * rows;
            let at = row * columns + col;
            (1..total).map(|step| if forward { (at + step) % total } else { (at + total - step) % total })
                .map(|i| (i / columns, i % columns))
                .find(|&(r, c)| grid.is_editable(c) && !grid.is_deleted(r))
        };
        self.commit(window, cx);
        if let Some((row, col)) = next {
            self.begin(row, col, window, cx, get);
        }
    }

    fn cancel<T: 'static>(&mut self, window: &mut Window, cx: &mut Context<T>) {
        self.subscriptions.clear();
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

    /// ⌘⌫: marks the selected row for deletion (or restores it).
    fn delete_selected_row<T: 'static>(&mut self, cx: &mut Context<T>) {
        self.grid.update(cx, |state, cx| {
            let Some(row) = RowsDelegate::selected_row(state.selection()) else { return };
            if !state.delegate().can_delete_rows() {
                return;
            }
            state.delegate_mut().toggle_delete(row);
            cx.notify();
        });
    }

    fn discard<T: 'static>(&mut self, cx: &mut Context<T>) {
        self.subscriptions.clear();
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().discard_edits();
            cx.notify();
        });
    }

    /// ⌘S: the SQL that saving runs, and Save.
    fn review<T: 'static>(&mut self, window: &mut Window, cx: &mut Context<T>) {
        self.commit(window, cx);
        if self.grid.read(cx).delegate().edits.is_empty() {
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
}

/// "Unsaved changes: 2 edited · 1 deleted  [Discard] [Review & Save…]" while a grid has edits.
fn edit_bar<T: 'static>(grid: &Entity<TableState<RowsDelegate>>, cx: &mut Context<T>, get: fn(&mut T) -> &mut GridEditor) -> Option<Div> {
    let edits = &grid.read(cx).delegate().edits;
    if edits.is_empty() {
        return None;
    }
    let summary = edits.summary();
    Some(
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().yellow.opacity(0.10))
            .child(div().flex_1().text_sm().child(format!("Unsaved changes: {summary}")))
            .child(Button::new("discard").ghost().small().label("Discard").on_click(cx.listener(move |this, _, _, cx| get(this).discard(cx))))
            .child(
                Button::new("review")
                    .primary()
                    .small()
                    .label(format!("Review & Save… ({})", keys::shortcut("secondary-s")))
                    .on_click(cx.listener(move |this, _, window, cx| get(this).review(window, cx))),
            ),
    )
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

/// What saving runs: a table's changes, or a script result's (to one or more tables).
enum Save {
    Table { connection: Arc<Connection>, table: TableInfo, columns: Vec<dbcore::ColumnInfo>, changes: Vec<dbcore::edit::RowChange> },
    Results { connection: Arc<Connection>, changes: Vec<TableChanges> },
}

impl Save {
    fn of(rows: &RowsDelegate) -> Result<Self, String> {
        if let Some((connection, table)) = rows.source() {
            return Ok(Self::Table { connection: connection.clone(), table: table.clone(), columns: rows.columns.clone(), changes: rows.changes() });
        }
        let (Some(links), Some(connection)) = (&rows.links, rows.connection()) else { return Err("Nothing to save.".into()) };
        let changes = links.changes(&rows.result_edits()).map_err(|e| e.to_string())?;
        Ok(Self::Results { connection: connection.clone(), changes })
    }

    fn preview(&self) -> Result<Vec<EditStatement>, String> {
        match self {
            Self::Table { connection, table, columns, changes } => connection.preview_changes(table, columns, changes),
            Self::Results { connection, changes } => connection.preview_result_changes(changes),
        }
        .map_err(|e| e.to_string())
    }
}

impl ReviewEdits {
    fn new(grid: Entity<TableState<RowsDelegate>>, cx: &mut Context<Self>) -> Self {
        let statements = Save::of(grid.read(cx).delegate()).and_then(|save| save.preview());
        Self { grid, statements, error: None, saving: false, task: None }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let save = match Save::of(self.grid.read(cx).delegate()) {
            Ok(save) => save,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.error = None;
        let grid = self.grid.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let table = matches!(save, Save::Table { .. });
            let result = match save {
                Save::Table { connection, table, columns, changes } => connection.apply_changes(table, columns, changes).await,
                Save::Results { connection, changes } => connection.apply_result_changes(changes).await,
            };
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        grid.update(cx, |state, cx| {
                            if table {
                                state.delegate_mut().discard_edits();
                                state.delegate_mut().refresh_rows(cx);
                            } else {
                                // Not re-run: a script may do more than select.
                                state.delegate_mut().apply_saved_edits();
                                state.refresh(cx);
                            }
                            cx.notify();
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

/// Where a script records its runs: the shared state store, and the connection and database.
#[derive(Clone)]
pub struct History {
    pub state: Rc<RefCell<StateStore>>,
    pub connection_id: String,
    pub database: String,
}

/// A run that returned rows, kept so it can move to a results tab of its own.
#[derive(Clone)]
pub struct Ran {
    pub sql: String,
    pub result: QueryResult,
    pub took: Duration,
}

/// A script asks for a results tab: of rows it already has (`ran`), or of running `sql` there.
#[derive(Clone)]
pub struct OpenResults {
    /// The script's name ("SQL 1"), for the tab's title.
    pub source: String,
    pub sql: String,
    pub ran: Option<Ran>,
}

/// A run finished, also when it failed (a script can create a table, then fail on a later
/// statement). Not sent for cancelled runs.
pub struct RunFinished {
    pub sql: String,
}

/// What running SQL shows: rows (with the inspector), rows affected, or the error, and a status
/// bar. A script tab has one under its editor; a results tab is one on its own.
pub struct Results {
    connection: Arc<Connection>,
    grid: Entity<TableState<RowsDelegate>>,
    inspector: Entity<Inspector>,
    outcome: Outcome,
    run_task: Option<Task<()>>,
    history: Option<History>,
    /// The last run that returned rows, for "Open in New Tab".
    last: Option<Ran>,
    /// Edits cells of tables whose primary key is in the result.
    editor: GridEditor,
    /// Finding the tables the rows come from (for editing and foreign key links).
    links_task: Option<Task<()>>,
    settle: Settle,
    menu: CellMenu,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RunFinished> for Results {}
/// Related rows to open (`TabEvent::OpenRelated`), passed on by the owning tab.
impl EventEmitter<TabEvent> for Results {}

impl Results {
    fn new(connection: Arc<Connection>, history: Option<History>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let grid = new_grid(window, cx);
        let inspector = cx.new(|cx| Inspector::new(grid.clone(), window, cx));
        // The grid's menu starts edits and opens related rows through the results.
        let this = cx.entity().downgrade();
        grid.update(cx, |state, _| {
            let rows = state.delegate_mut();
            let edit = this.clone();
            rows.begin_edit = Some(Rc::new(move |row, col, window, cx| {
                edit.update(cx, |r, cx| r.editor.begin(row, col, window, cx, |r: &mut Self| &mut r.editor)).ok();
            }));
            rows.open_related = Some(Rc::new(move |related, _, cx| {
                this.update(cx, |_, cx| cx.emit(TabEvent::OpenRelated(related))).ok();
            }));
        });
        let subscriptions = vec![
            cx.observe_global::<inspector::ShowInspector>(|_, cx| cx.notify()),
            cx.observe(&grid, |_, _, cx| cx.notify()),
            cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| match *event {
                TableEvent::DoubleClickedCell(row, col) => {
                    this.editor.begin(row, col, window, cx, |this: &mut Self| &mut this.editor);
                }
                TableEvent::RightClickedCell(row, col) => {
                    this.menu.open(&this.grid, row, col, window, cx, |this: &mut Self| &mut this.menu);
                }
                _ => {}
            }),
        ];
        Self {
            connection,
            editor: GridEditor::new(grid.clone()),
            links_task: None,
            grid,
            inspector,
            outcome: Outcome::Idle,
            run_task: None,
            history,
            last: None,
            settle: Settle::default(),
            menu: CellMenu::default(),
            _subscriptions: subscriptions,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.outcome, Outcome::Running(_))
    }

    /// The rows shown, when the last run returned some.
    pub fn shown_rows(&self) -> Option<&Ran> {
        self.last.as_ref().filter(|_| matches!(self.outcome, Outcome::Rows { .. }))
    }

    pub fn has_unsaved_edits(&self, cx: &App) -> bool {
        !self.grid.read(cx).delegate().edits.is_empty()
    }

    fn run(&mut self, sql: String, cx: &mut Context<Self>) {
        if self.is_running() || sql.trim().is_empty() {
            return;
        }
        // Running replaces the rows the edits point at.
        if self.has_unsaved_edits(cx) {
            self.editor.notice = Some("Save or discard your changes first.".into());
            cx.notify();
            return;
        }
        self.editor.notice = None;
        let started = Instant::now();
        self.outcome = Outcome::Running(started);
        let connection = self.connection.clone();
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.execute_limited(sql.clone(), Some(SCRIPT_ROW_LIMIT)).await;
            this.update(cx, |this, cx| {
                let took = started.elapsed();
                this.record(&sql, &result, took);
                let cancelled = matches!(result, Err(dbcore::Error::Cancelled));
                this.show(sql.clone(), result, took, cx);
                if !cancelled {
                    cx.emit(RunFinished { sql });
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Adds the run to the connection's history (cancelled runs aren't kept).
    fn record(&self, sql: &str, result: &dbcore::Result<QueryResult>, took: Duration) {
        let Some(history) = &self.history else { return };
        let (rows, error) = match result {
            Ok(r) if r.columns.is_empty() => (r.rows_affected, None),
            Ok(r) => (Some(r.total_count.unwrap_or(r.rows.len() as u64)), None),
            Err(dbcore::Error::Cancelled) => return,
            Err(e) => (None, Some(e.to_string())),
        };
        let entry = NewHistoryEntry {
            connection_id: history.connection_id.clone(),
            database: history.database.clone(),
            sql: sql.to_string(),
            duration: Some(took),
            rows,
            error,
        };
        if let Err(e) = history.state.borrow_mut().add_history(entry) {
            log::warn!("couldn’t record the query: {e}");
        }
    }

    fn show(&mut self, sql: String, result: dbcore::Result<QueryResult>, took: Duration, cx: &mut Context<Self>) {
        match result {
            Ok(result) if !result.columns.is_empty() => self.show_rows(Ran { sql, result, took }, cx),
            Ok(result) => self.outcome = Outcome::Affected { rows: result.rows_affected, took },
            Err(dbcore::Error::Cancelled) => self.outcome = Outcome::Cancelled,
            Err(e) => self.outcome = Outcome::Failed(e.to_string()),
        }
        cx.notify();
    }

    fn show_rows(&mut self, ran: Ran, cx: &mut Context<Self>) {
        let result = ran.result.clone();
        let origins = result.origins.clone();
        self.outcome = Outcome::Rows { took: ran.took, truncated: result.truncated };
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().show_result(self.connection.clone(), result);
            state.refresh(cx);
        });
        // Which tables the rows come from: their cells can be edited (when the table's key is in
        // the result) and their foreign keys followed. Until then (or if it fails) read-only.
        self.links_task = (!origins.is_empty()).then(|| {
            let connection = self.connection.clone();
            cx.spawn(async move |this, cx| {
                let Ok(links) = connection.describe_result(origins).await else { return };
                this.update(cx, |this, cx| {
                    this.grid.update(cx, |state, cx| {
                        state.delegate_mut().links = Some(links);
                        cx.notify();
                    });
                })
                .ok();
            })
        });
        self.last = Some(ran);
        self.settle.start();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.is_running() {
            return;
        }
        let connection = self.connection.clone();
        cx.spawn(async move |_, _| connection.cancel().await).detach();
    }

    fn status(&self, cx: &App) -> String {
        match &self.outcome {
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
        }
    }
}

impl Render for Results {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settle.tick(window, cx, |this: &mut Self| &mut this.settle);
        let body = match &self.outcome {
            Outcome::Idle => centered().text_color(cx.theme().muted_foreground).child("Run a query to see its results").into_any_element(),
            Outcome::Running(_) => centered().child(Spinner::new()).into_any_element(),
            Outcome::Rows { .. } => {
                grid_with_inspector(data_table(&self.grid, self.settle.0, &self.menu, cx), &self.inspector, cx)
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
        let notice = self.editor.notice.clone();
        let edit_bar = edit_bar(&self.grid, cx, |this: &mut Self| &mut this.editor);
        v_flex()
            .size_full()
            .key_context("Results")
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| copy_selection(&this.grid, false, cx)))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| copy_selection(&this.grid, true, cx)))
            .on_action(cx.listener(|this, _: &CancelEdit, window, cx| this.editor.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &StartEdit, window, cx| {
                this.editor.start(window, cx, |this: &mut Self| &mut this.editor);
            }))
            .on_action(cx.listener(|this, _: &NextCell, window, cx| this.editor.step(true, window, cx, |this: &mut Self| &mut this.editor)))
            .on_action(cx.listener(|this, _: &PreviousCell, window, cx| this.editor.step(false, window, cx, |this: &mut Self| &mut this.editor)))
            .on_action(cx.listener(|this, _: &DeleteRow, _, cx| this.editor.delete_selected_row(cx)))
            .on_action(cx.listener(|this, _: &SaveEdits, window, cx| this.editor.review(window, cx)))
            .children(notice.map(|e| div().px_3().py_1().text_xs().text_color(cx.theme().red).child(e)))
            .child(div().flex_1().min_h_0().child(body))
            .children(edit_bar)
            .child(status_bar(self.status(cx), cx))
            .children(self.menu.render())
    }
}

fn took(duration: Duration) -> String {
    if duration.as_millis() < 1000 {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

fn stop_button(results: &Entity<Results>) -> Button {
    let results = results.clone();
    Button::new("stop")
        .small()
        .icon(IconName::Square)
        .label("Stop")
        .on_click(move |_, _, cx| results.update(cx, |r, cx| r.cancel(cx)))
}

/// Passes what `results` asks on to the tab's owner: related rows to open, and DDL (the tables
/// may have changed).
fn forward_results_events<T: EventEmitter<TabEvent>>(results: &Entity<Results>, cx: &mut Context<T>) -> [Subscription; 2] {
    [
        cx.subscribe(results, |_, _, event: &RunFinished, cx| {
            if dbcore::dialect::changes_schema(&event.sql) {
                cx.emit(TabEvent::SchemaMayHaveChanged);
            }
        }),
        cx.subscribe(results, |_, _, event: &TabEvent, cx| {
            if let TabEvent::OpenRelated(related) = event {
                cx.emit(TabEvent::OpenRelated(related.clone()));
            }
        }),
    ]
}

pub struct ScriptTab {
    /// "SQL 1", "SQL 2"…
    pub name: String,
    /// The database it runs in (shown in the header).
    pub target: String,
    editor: Entity<EditorState>,
    results: Entity<Results>,
    _catalog_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ScriptTab {
    pub fn new(
        connection: Arc<Connection>,
        name: String,
        target: String,
        history: Option<History>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let catalog: SharedCatalog = Rc::default();
        let completion = SqlCompletion { catalog: catalog.clone(), dialect: Dialect(connection.config().kind) };
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).language(highlight::SQL).line_number(true);
            state.set_highlighter_factory(highlight::factory(), cx);
            state.lsp_mut().completion_provider = Some(Rc::new(completion));
            state
        });
        let results = cx.new(|cx| Results::new(connection.clone(), history, window, cx));
        let mut subscriptions = vec![cx.observe_global::<EditorFontSize>(|_, cx| cx.notify()), cx.observe(&results, |_, _, cx| cx.notify())];
        subscriptions.extend(forward_results_events(&results, cx));
        // Tables and columns for completion. Keywords complete meanwhile, and if this fails.
        let catalog_task = cx.spawn({
            let connection = connection.clone();
            async move |_, _| {
                let schemas = connection.list_schemas().await;
                let columns = connection.list_columns().await;
                match (schemas, columns) {
                    (Ok(schemas), Ok(columns)) => *catalog.borrow_mut() = Some(Rc::new(Catalog::new(schemas, columns))),
                    (Err(e), _) | (_, Err(e)) => log::warn!("couldn’t load the completion catalog: {e}"),
                }
            }
        });
        Self { name, target, editor, results, _catalog_task: catalog_task, _subscriptions: subscriptions }
    }

    pub fn has_unsaved_edits(&self, cx: &App) -> bool {
        self.results.read(cx).has_unsaved_edits(cx)
    }

    pub fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    /// The whole script.
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.editor.update(cx, |e, cx| e.set_value(text, window, cx));
    }

    /// Puts a query from the history in the editor: as the script when it's empty, else after it.
    fn insert_from_history(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.text(cx);
        let text = if current.trim().is_empty() { sql.to_string() } else { format!("{}\n\n{sql}", current.trim_end()) };
        self.set_text(&text, window, cx);
        let focus = self.editor_focus(cx);
        focus.focus(window, cx);
    }

    fn history_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let history = self.results.read(cx).history.clone();
        Button::new("history")
            .ghost()
            .small()
            .label("History")
            .dropdown_caret(true)
            .disabled(history.is_none())
            .dropdown_menu(move |mut menu, _, _| {
                let Some(history) = history.clone() else { return menu };
                // Read when the menu opens, so it's always current.
                let entries = history.state.borrow().history(&history.connection_id, 25).unwrap_or_default();
                if entries.is_empty() {
                    return menu.item(PopupMenuItem::new("No queries yet").disabled(true));
                }
                for entry in entries {
                    let one_line = one_line(&entry.sql);
                    let mut label: String = one_line.chars().take(70).collect();
                    if one_line.chars().count() > 70 {
                        label.push('…');
                    }
                    if entry.error.is_some() {
                        label.push_str("  · failed");
                    }
                    let (this, sql) = (this.clone(), entry.sql.clone());
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        this.update(cx, |tab, cx| tab.insert_from_history(&sql, window, cx)).ok();
                    }));
                }
                let state = history.state.clone();
                let id = history.connection_id.clone();
                menu.separator().item(PopupMenuItem::new("Clear History").on_click(move |_, _, _| {
                    if let Err(e) = state.borrow_mut().clear_history(&id) {
                        log::warn!("couldn’t clear the history: {e}");
                    }
                }))
            })
    }

    /// The selection when there is one, else the whole script.
    fn sql(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let selected = editor.selected_text().to_string();
        if selected.trim().is_empty() { editor.value().to_string() } else { selected }
    }

    fn run(&mut self, _: &RunScript, _: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql(cx);
        // ⌘↩ while completing: the suggestions would stay over the results.
        self.editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
        self.results.update(cx, |results, cx| results.run(sql, cx));
    }

    /// ⇧⌘↩: runs the selection or the script in a results tab of its own.
    fn run_in_new_tab(&mut self, _: &RunInNewTab, _: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql(cx);
        if sql.trim().is_empty() {
            return;
        }
        self.editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
        cx.emit(TabEvent::OpenResults(OpenResults { source: self.name.clone(), sql, ran: None }));
    }

    /// Moves the rows shown to a results tab, so the next run doesn't replace them.
    fn open_in_new_tab(&mut self, cx: &mut Context<Self>) {
        let Some(ran) = self.results.read(cx).shown_rows().cloned() else { return };
        cx.emit(TabEvent::OpenResults(OpenResults { source: self.name.clone(), sql: ran.sql.clone(), ran: Some(ran) }));
    }
}

impl EventEmitter<TabEvent> for ScriptTab {}


impl Render for ScriptTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.results.read(cx);
        let running = results.is_running();
        let has_rows = results.shown_rows().is_some();
        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().font_semibold().child(self.name.clone()))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(self.target.clone()))
            .child(div().flex_1())
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(if running {
                format!("{} to stop", keys::shortcut("secondary-."))
            } else {
                format!(
                    "{} runs the selection or the script, {} in a new tab",
                    keys::shortcut("secondary-enter"),
                    keys::shortcut("shift-secondary-enter")
                )
            }))
            .child(self.history_menu(cx))
            .when(has_rows, |header| {
                header.child(
                    Button::new("open-results")
                        .ghost()
                        .small()
                        .icon(Icon::new(AssetIcon::SquareArrowOutUpRight))
                        .tooltip("Open Results in New Tab")
                        .on_click(cx.listener(|this, _, _, cx| this.open_in_new_tab(cx))),
                )
            })
            .child(inspector_button(cx))
            .child(if running {
                stop_button(&self.results)
            } else {
                Button::new("run")
                    .small()
                    .primary()
                    .icon(IconName::Play)
                    .label("Run")
                    .on_click(cx.listener(|this, _, window, cx| this.run(&RunScript, window, cx)))
            });

        v_flex()
            .key_context("ScriptTab")
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::run_in_new_tab))
            .on_action(cx.listener(|this, _: &CancelScript, _, cx| this.results.update(cx, |r, cx| r.cancel(cx))))
            .on_action(|_: &ZoomIn, _, cx| EditorFontSize::set(EditorFontSize::get(cx) + 1., cx))
            .on_action(|_: &ZoomOut, _, cx| EditorFontSize::set(EditorFontSize::get(cx) - 1., cx))
            .on_action(|_: &ResetZoom, _, cx| EditorFontSize::set(EditorFontSize::DEFAULT, cx))
            .size_full()
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("script-split")
                        .child(resizable_panel().size(px(260.)).size_range(px(80.)..px(2000.)).child(
                            Editor::new(&self.editor)
                                .size_full()
                                .border_0()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(EditorFontSize::get(cx))),
                        ))
                        .child(resizable_panel().child(self.results.clone())),
                ),
            )
    }
}

fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

// MARK: results tab

/// A script's results in a tab of their own (⇧⌘↩, or "Open in New Tab"). Read-only; Re-run (⌘↩)
/// runs the same SQL again. Not reopened at launch: running a script by itself could write.
pub struct ResultTab {
    pub title: String,
    /// The database it ran in (shown in the header).
    target: String,
    sql: String,
    results: Entity<Results>,
    _subscriptions: Vec<Subscription>,
}

impl ResultTab {
    pub fn new(
        connection: Arc<Connection>,
        title: String,
        target: String,
        request: OpenResults,
        history: Option<History>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let OpenResults { sql, ran, .. } = request;
        let results = cx.new(|cx| Results::new(connection, history, window, cx));
        let mut subscriptions = vec![cx.observe(&results, |_, _, cx| cx.notify())];
        subscriptions.extend(forward_results_events(&results, cx));
        results.update(cx, |results, cx| match ran {
            Some(ran) => results.show_rows(ran, cx),
            None => results.run(sql.clone(), cx),
        });
        Self { title, target, sql, results, _subscriptions: subscriptions }
    }
}

impl ResultTab {
    pub fn has_unsaved_edits(&self, cx: &App) -> bool {
        self.results.read(cx).has_unsaved_edits(cx)
    }

    fn rerun(&mut self, cx: &mut Context<Self>) {
        let sql = self.sql.clone();
        self.results.update(cx, |r, cx| r.run(sql, cx));
    }
}

impl EventEmitter<TabEvent> for ResultTab {}

impl Render for ResultTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.results.read(cx).is_running();
        let sql: SharedString = self.sql.clone().into();
        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_none().font_semibold().child(self.title.clone()))
            .child(div().flex_none().text_xs().text_color(cx.theme().muted_foreground).child(self.target.clone()))
            .child(
                div()
                    .id("result-sql")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(one_line(&self.sql))
                    .tooltip(move |window, cx| Tooltip::new(sql.clone()).build(window, cx)),
            )
            .child(inspector_button(cx))
            .child(if running {
                stop_button(&self.results)
            } else {
                Button::new("rerun")
                    .small()
                    .icon(Icon::new(AssetIcon::RotateCw))
                    .label("Re-run")
                    .tooltip(format!("Run the query again ({})", keys::shortcut("secondary-enter")))
                    .on_click(cx.listener(|this, _, _, cx| this.rerun(cx)))
            });
        v_flex()
            .key_context("ResultTab")
            .on_action(cx.listener(|this, _: &RunScript, _, cx| this.rerun(cx)))
            .on_action(cx.listener(|this, _: &CancelScript, _, cx| this.results.update(cx, |r, cx| r.cancel(cx))))
            .size_full()
            .child(header)
            .child(div().flex_1().min_h_0().child(self.results.clone()))
    }
}
