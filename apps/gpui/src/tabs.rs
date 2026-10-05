//! What the right pane shows: a table's rows, or a SQL script with its results.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dbcore::{Connection, QueryResult, RowQuery, TableInfo};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::grid::{PAGE_SIZE, RowsDelegate};
use crate::sql_highlight;

/// Rows kept from one script result (like the macOS app); the rest are counted, not shown.
pub const SCRIPT_ROW_LIMIT: u32 = 10_000;

actions!(dbear, [RunScript, CancelScript]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // More specific than the editor's own ⌘↩ (insert a line), so running wins inside scripts.
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab > Input")),
        KeyBinding::new("secondary-enter", RunScript, Some("ScriptTab")),
        KeyBinding::new("secondary-.", CancelScript, Some("ScriptTab")),
    ]);
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

pub struct TableTab {
    pub table: TableInfo,
    /// Replaced by the next table opened with a single click; double-click keeps it.
    pub preview: bool,
    rows: Load,
    grid: Entity<TableState<RowsDelegate>>,
    _task: Task<()>,
    _observe: Subscription,
}

impl TableTab {
    pub fn new(
        connection: Arc<Connection>,
        table: TableInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = cx.new(|cx| TableState::new(RowsDelegate::default(), window, cx).row_selectable(true));
        let observe = cx.observe(&grid, |_, _, cx| cx.notify());
        let task = cx.spawn({
            let table = table.clone();
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
        Self { table, preview: true, rows: Load::Loading, grid, _task: task, _observe: observe }
    }

    pub fn title(&self) -> String {
        self.table.name.clone()
    }
}

impl Render for TableTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.rows {
            Load::Loading => centered().child(Spinner::new()).into_any_element(),
            Load::Failed(e) => message("Couldn’t Load Rows", e.clone(), cx).into_any_element(),
            Load::Loaded => DataTable::new(&self.grid).stripe(true).bordered(false).into_any_element(),
        };
        let status = match &self.rows {
            Load::Loaded => {
                let grid = self.grid.read(cx).delegate();
                let mut status = rows_status(grid);
                if grid.loading_more {
                    status.push_str(" · Loading more…");
                } else if let Some(error) = &grid.load_more_error {
                    status.push_str(&format!(" · Couldn’t load more: {error}"));
                }
                status
            }
            Load::Loading => "Loading…".into(),
            Load::Failed(_) => String::new(),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().font_semibold().child(self.table.name.clone()))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(self.table.schema.clone())),
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
    outcome: Outcome,
    run_task: Option<Task<()>>,
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
        let grid = cx.new(|cx| TableState::new(RowsDelegate::default(), window, cx).row_selectable(true));
        Self {
            name,
            target,
            connection,
            editor,
            grid,
            outcome: Outcome::Idle,
            run_task: None,
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
                self.grid.update(cx, |state, cx| {
                    state.delegate_mut().show_result(result);
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
            .child(run);

        let results = match &self.outcome {
            Outcome::Idle => centered().text_color(cx.theme().muted_foreground).child("Run a query to see its results").into_any_element(),
            Outcome::Running(_) => centered().child(Spinner::new()).into_any_element(),
            Outcome::Rows { .. } => DataTable::new(&self.grid).stripe(true).bordered(false).into_any_element(),
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
