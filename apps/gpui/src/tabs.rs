//! What the right pane shows: a table (rows or structure), or a SQL script with its results.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dbcore::export::CopyFormat;
use dbcore::{Connection, QueryResult, RowQuery, TableInfo, TableStructure};
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::{PAGE_SIZE, RowsDelegate, copy};
use crate::inspector::{self, Inspector};
use crate::sql_highlight;

/// Rows kept from one script result (like the macOS app); the rest are counted, not shown.
pub const SCRIPT_ROW_LIMIT: u32 = 10_000;

actions!(dbear, [RunScript, CancelScript, CopySelection, CopySelectionWithHeaders, ToggleInspector]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // More specific than the editor's own ⌘↩ (insert a line), so running wins inside scripts.
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab > Input")),
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab")),
        KeyBinding::new("secondary-.", CancelScript, Some("ScriptTab")),
        KeyBinding::new("secondary-c", CopySelection, Some("DataTable")),
        KeyBinding::new("shift-secondary-c", CopySelectionWithHeaders, Some("DataTable")),
        KeyBinding::new("alt-secondary-i", ToggleInspector, None),
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
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TableTab {
    pub fn new(connection: Arc<Connection>, table: TableInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let grid = new_grid(window, cx);
        let inspector = cx.new(|cx| Inspector::new(grid.clone(), cx));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows, e.g. status = 'paid' and total > 10"));
        let subscriptions = vec![
            cx.observe(&grid, |_, _, cx| cx.notify()),
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
            _tasks: vec![task],
            _subscriptions: subscriptions,
        }
    }

    pub fn title(&self) -> String {
        self.table.name.clone()
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
        let error = grid.reload_error.clone();
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
                let table = DataTable::new(&self.grid).stripe(true).bordered(false).into_any_element();
                grid_with_inspector(table, &self.inspector, cx)
            }
        };
        v_flex()
            .size_full()
            .child(filter_bar)
            .children(error.map(|e| div().px_3().py_1().text_xs().text_color(cx.theme().red).child(e)))
            .child(div().flex_1().min_h_0().child(body))
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
        let mode_button = |id: &'static str, label: &'static str, m: Mode, cx: &mut Context<Self>| {
            Button::new(id).ghost().small().label(label).selected(mode == m).on_click(cx.listener(move |this, _, _, cx| this.set_mode(m, cx)))
        };
        v_flex()
            .key_context("TableTab")
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| copy_selection(&this.grid, false, cx)))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| copy_selection(&this.grid, true, cx)))
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
                    .child(mode_button("mode-data", "Data", Mode::Data, cx))
                    .child(mode_button("mode-structure", "Structure", Mode::Structure, cx))
                    .when(mode == Mode::Data, |h| h.child(inspector_button(cx))),
            )
            .child(div().flex_1().min_h_0().child(body))
            .child(status_bar(status, cx))
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
                grid_with_inspector(DataTable::new(&self.grid).stripe(true).bordered(false).into_any_element(), &self.inspector, cx)
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
