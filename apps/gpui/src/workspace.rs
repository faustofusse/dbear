//! The main window: connections | schemas and tables | rows, like the macOS app.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbcore::secrets::{self, KeyringSecretStore};
use dbcore::{Connection, ConnectionConfig, ConnectionStore, Schema, TableInfo, TableKind};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::RowsDelegate;

/// Rows fetched when a table opens. Paging comes later.
const PAGE_SIZE: u32 = 500;

enum Load<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(String),
}

struct RowsInfo {
    shown: usize,
    total: Option<u64>,
    columns: usize,
}

pub struct Workspace {
    connections: Vec<ConnectionConfig>,
    /// Why the saved connections or passwords aren't available, shown under the list.
    notice: Option<String>,
    secrets: Option<Arc<KeyringSecretStore>>,
    open: HashMap<String, Arc<Connection>>,
    selected: Option<String>,
    schemas: Load<Vec<Schema>>,
    collapsed: HashSet<String>,
    table: Option<TableInfo>,
    rows: Load<RowsInfo>,
    grid: Entity<TableState<RowsDelegate>>,
    // Dropping a task cancels it, so switching selection abandons the previous load.
    schemas_task: Option<Task<()>>,
    rows_task: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut notices = Vec::new();
        let mut connections = match open_store() {
            Ok(store) => store.connections().to_vec(),
            Err(e) => {
                notices.push(format!("Couldn’t open saved connections: {e}"));
                Vec::new()
            }
        };
        if connections.is_empty() {
            notices.push("No saved connections yet; showing the samples.".into());
            connections = dbcore::mock::connections();
        }
        let secrets = match KeyringSecretStore::new() {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                notices.push(format!("Saved passwords are unavailable ({e})."));
                None
            }
        };
        let grid = cx.new(|cx| TableState::new(RowsDelegate::default(), window, cx).row_selectable(true));
        Self {
            connections,
            notice: (!notices.is_empty()).then(|| notices.join("\n")),
            secrets,
            open: HashMap::new(),
            selected: None,
            schemas: Load::Idle,
            collapsed: HashSet::new(),
            table: None,
            rows: Load::Idle,
            grid,
            schemas_task: None,
            rows_task: None,
        }
    }

    fn selected_connection(&self) -> Option<&ConnectionConfig> {
        let id = self.selected.as_deref()?;
        self.connections.iter().find(|c| c.id == id)
    }

    fn select_connection(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        let Some(config) = self.connections.iter().find(|c| c.id == id).cloned() else { return };
        self.selected = Some(id.clone());
        self.collapsed.clear();
        self.close_table(cx);
        self.load_schemas(config, cx);
    }

    fn load_schemas(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        self.schemas = Load::Loading;
        let id = config.id.clone();
        let existing = self.open.get(&id).cloned();
        let secrets = self.secrets.clone();
        self.schemas_task = Some(cx.spawn(async move |this, cx| {
            let connection = match existing {
                Some(connection) => connection,
                None => {
                    // Reading the keyring can block (or prompt), so keep it off the UI thread.
                    let config = cx
                        .background_executor()
                        .spawn(async move {
                            match secrets {
                                Some(store) => secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config),
                                None => config,
                            }
                        })
                        .await;
                    Arc::new(Connection::new(config))
                }
            };
            let result = connection.list_schemas().await;
            this.update(cx, |this, cx| {
                if this.selected.as_deref() != Some(id.as_str()) {
                    return;
                }
                this.schemas = match result {
                    Ok(schemas) => {
                        this.open.insert(id, connection);
                        Load::Loaded(schemas)
                    }
                    Err(e) => Load::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_table(&mut self, cx: &mut Context<Self>) {
        self.table = None;
        self.rows = Load::Idle;
        self.rows_task = None;
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().set(Vec::new(), Vec::new());
            state.refresh(cx);
        });
    }

    fn open_table(&mut self, table: TableInfo, cx: &mut Context<Self>) {
        if self.table.as_ref() == Some(&table) {
            return;
        }
        let Some(connection) = self.selected.as_ref().and_then(|id| self.open.get(id)).cloned() else { return };
        self.table = Some(table.clone());
        self.rows = Load::Loading;
        self.rows_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.fetch_rows(table.clone(), PAGE_SIZE, 0).await;
            this.update(cx, |this, cx| {
                if this.table.as_ref() != Some(&table) {
                    return;
                }
                this.rows = match result {
                    Ok(result) => {
                        let info = RowsInfo {
                            shown: result.rows.len(),
                            total: result.total_count,
                            columns: result.columns.len(),
                        };
                        this.grid.update(cx, |state, cx| {
                            state.delegate_mut().set(result.columns, result.rows);
                            state.refresh(cx);
                            if info.shown > 0 {
                                state.scroll_to_row(0, cx);
                            }
                        });
                        Load::Loaded(info)
                    }
                    Err(e) => Load::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle_schema(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(name) {
            self.collapsed.insert(name.to_string());
        }
        cx.notify();
    }

    // MARK: columns

    fn render_connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut groups: Vec<(String, Vec<&ConnectionConfig>)> = Vec::new();
        for c in &self.connections {
            match groups.iter_mut().find(|(g, _)| *g == c.group) {
                Some((_, list)) => list.push(c),
                None => groups.push((c.group.clone(), vec![c])),
            }
        }
        let mut list = v_flex().id("connections").size_full().p_2().gap_px().overflow_y_scrollbar();
        for (group, connections) in groups {
            let title = if group.is_empty() { "Connections".to_string() } else { group };
            list = list.child(section_header(title, theme.muted_foreground));
            for c in connections {
                let selected = self.selected.as_deref() == Some(c.id.as_str());
                let connected = self.open.contains_key(&c.id);
                let id = c.id.clone();
                list = list.child(
                    row(SharedString::from(format!("conn-{}", c.id)), selected, cx)
                        .child(Icon::new(IconName::HardDrive).small().text_color(if selected {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        }))
                        .child(div().flex_1().truncate().child(c.name.clone()))
                        .when(connected, |row| row.child(div().size_1p5().rounded_full().bg(theme.green)))
                        .tooltip({
                            let summary = SharedString::from(c.summary());
                            move |window, cx| Tooltip::new(summary.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.select_connection(id.clone(), cx))),
                );
            }
        }
        if let Some(notice) = &self.notice {
            list = list.child(div().mt_4().px_2().text_xs().text_color(theme.muted_foreground).child(notice.clone()));
        }
        div().size_full().bg(theme.sidebar).child(list)
    }

    fn render_tables(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(connection) = self.selected_connection() else {
            return v_flex().size_full().bg(theme.background);
        };
        let subtitle = match &self.schemas {
            Load::Loaded(schemas) => {
                let tables: usize = schemas.iter().map(|s| s.tables.len()).sum();
                format!("{} · {tables} tables", plural(schemas.len(), "schema"))
            }
            Load::Loading => "Loading…".into(),
            _ => connection.summary(),
        };
        let header = v_flex()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().font_semibold().truncate().child(connection.name.clone()))
            .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(subtitle));

        let body = match &self.schemas {
            Load::Idle | Load::Loading => centered().child(Spinner::new()).into_any_element(),
            Load::Failed(message) => centered()
                .p_4()
                .gap_2()
                .child(Icon::new(IconName::TriangleAlert).text_color(theme.warning))
                .child(div().font_semibold().child("Couldn’t Connect"))
                .child(div().text_sm().text_color(theme.muted_foreground).text_center().child(message.clone()))
                .into_any_element(),
            Load::Loaded(schemas) => {
                let mut list = v_flex().id("tables").size_full().p_2().gap_px().overflow_y_scrollbar();
                for schema in schemas {
                    let collapsed = self.collapsed.contains(&schema.name);
                    let name = schema.name.clone();
                    list = list.child(
                        h_flex()
                            .id(SharedString::from(format!("schema-{}", schema.name)))
                            .mt_2()
                            .px_1()
                            .gap_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).xsmall())
                            .child(schema.name.clone())
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_schema(&name, cx))),
                    );
                    if collapsed {
                        continue;
                    }
                    for table in &schema.tables {
                        let selected = self.table.as_ref() == Some(table);
                        let open = table.clone();
                        let icon = if table.kind == TableKind::View { IconName::Eye } else { IconName::FileText };
                        list = list.child(
                            row(SharedString::from(format!("table-{}", table.qualified_name())), selected, cx)
                                .child(Icon::new(icon).small().text_color(if selected {
                                    theme.primary
                                } else {
                                    theme.muted_foreground
                                }))
                                .child(div().flex_1().truncate().child(table.name.clone()))
                                .children(table.estimated_row_count.map(|n| {
                                    div().text_xs().text_color(theme.muted_foreground).child(count(n))
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| this.open_table(open.clone(), cx))),
                        );
                    }
                }
                list.into_any_element()
            }
        };
        v_flex().size_full().bg(theme.background).child(header).child(div().flex_1().min_h_0().child(body))
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(table) = &self.table else {
            return centered()
                .bg(theme.background)
                .text_2xl()
                .text_color(theme.muted_foreground.opacity(0.6))
                .child("No Table Selected")
                .into_any_element();
        };
        let body = match &self.rows {
            Load::Idle | Load::Loading => centered().child(Spinner::new()).into_any_element(),
            Load::Failed(message) => centered()
                .p_4()
                .gap_2()
                .child(div().font_semibold().child("Couldn’t Load Rows"))
                .child(div().text_sm().text_color(theme.muted_foreground).text_center().child(message.clone()))
                .into_any_element(),
            Load::Loaded(_) => DataTable::new(&self.grid).stripe(true).bordered(false).into_any_element(),
        };
        let status = match &self.rows {
            Load::Loaded(info) => {
                let rows = match info.total {
                    Some(total) if total as usize > info.shown => {
                        format!("{} of {}", count(info.shown as u64), plural(total as usize, "row"))
                    }
                    _ => plural(info.shown, "row"),
                };
                format!("{rows} · {}", plural(info.columns, "column"))
            }
            Load::Loading => "Loading…".into(),
            _ => String::new(),
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_semibold().child(table.name.clone()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(table.schema.clone())),
            )
            .child(div().flex_1().min_h_0().child(body))
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.status_bar)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_resizable("workspace")
            .child(resizable_panel().size(px(240.)).size_range(px(180.)..px(400.)).child(self.render_connections(cx)))
            .child(resizable_panel().size(px(300.)).size_range(px(200.)..px(520.)).child(self.render_tables(cx)))
            .child(resizable_panel().child(self.render_rows(cx)))
    }
}

fn open_store() -> dbcore::Result<ConnectionStore> {
    // DBEAR_CONNECTIONS_FILE points at another store (handy for testing), as in the macOS app.
    match std::env::var_os("DBEAR_CONNECTIONS_FILE") {
        Some(path) => ConnectionStore::open(path),
        None => ConnectionStore::open_default(),
    }
}

fn section_header(title: String, color: Hsla) -> impl IntoElement {
    div().mt_3().mb_1().px_2().text_xs().font_semibold().text_color(color).child(title)
}

/// A selectable list row, Mail-style: soft background when selected.
fn row(id: SharedString, selected: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .when(selected, |row| row.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
        .when(!selected, |row| row.hover(|row| row.bg(theme.list_hover)))
}

fn centered() -> Div {
    v_flex().size_full().items_center().justify_center()
}

fn count(n: u64) -> String {
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

fn plural(n: usize, word: &str) -> String {
    format!("{} {word}{}", count(n as u64), if n == 1 { "" } else { "s" })
}
