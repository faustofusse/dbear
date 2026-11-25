//! The main window: connections | schemas and tables | tabs (tables and SQL scripts), like the macOS app.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use dbcore::dialect::Dialect;
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Schema, TableInfo, TableKind};
use dbservice::{Options, Part, SaveRequest, SecretKind, Service};
use serde::{Deserialize, Serialize};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab as TabItem, TabBar};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::backup::{Backups, DumpDialog, DumpPreset, Restored, RestoreDialog};
use crate::connection_editor::ConnectionEditor;
use crate::import_dialog::ImportDialog;
use crate::new_database::{CreateRequested, NewDatabaseDialog};
use crate::grid::{RelatedRows, copy};
use crate::users::{self, RoleCreated, UsersState, UsersTab};
use crate::menus::MenuState;
use crate::palette::{self, OpenPalette, Palette, Target};
use dbcore::palette::{Candidate, Kind};
use crate::tabs::{AddRow, EditorFontSize, OpenResults, ResultTab, ScriptTab, ScriptTarget, ShowData, ShowStructure, TabEvent, TableTab, count, plural};

enum Load<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(String),
}

/// How the tables of a connection entered in the palette (Tab) are coming along.
#[derive(Clone)]
pub enum PaletteTables {
    Loading,
    Failed(String),
    Ready,
}

/// One connection per (connection id, database): Postgres can't switch databases on a session.
pub(crate) type TargetKey = dbservice::Target;

actions!(
    dbear,
    [
        NewScript, CloseTab, PreviousTab, NextTab, SelectTab1, SelectTab2, SelectTab3, SelectTab4, SelectTab5, SelectTab6,
        SelectTab7, SelectTab8, LastTab, NewConnection, EditConnection, ImportConnections, ShowUsers, ToggleSidebar,
        SelectPrevious, SelectNext, DeleteSelected, Disconnect, DisconnectAll
    ]
);

/// A list whose selection the arrow keys move, and which scrolls to it afterwards.
#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    Connections,
    Tables,
    Roles,
}

/// A row of the connections list: a connection, or one of its databases.
type SidebarRow = (String, Option<String>);

/// How often open connections are asked whether the server still has them (no network I/O).
const MONITOR_INTERVAL: Duration = Duration::from_secs(3);

/// A tab being dragged to a new place in the tab bar.
#[derive(Clone)]
struct DraggedTab {
    ix: usize,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_md()
            .text_sm()
            .child(self.title.clone())
    }
}

/// Key of the open tabs in the state store.
const SESSION_KEY: &str = "gpui.session";
/// Key of the script editor's text size in the state store.
const FONT_SIZE_KEY: &str = "gpui.editor_font_size";

/// What's reopened at launch: the selection and the tabs, in order.
#[derive(Serialize, Deserialize, Default)]
struct Session {
    selected: Option<(String, String)>,
    active: usize,
    tabs: Vec<SavedTab>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SavedTab {
    Table {
        connection: String,
        database: String,
        schema: String,
        table: String,
        view: bool,
        /// Rows opened through a foreign key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
    },
    Script { connection: String, database: String, name: String, sql: String },
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-t", NewScript, Some("Workspace")),
        KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
        // Like the macOS app (⌘⇧[ / ⌘⇧]), plus the usual Linux and Windows ones.
        KeyBinding::new("secondary-shift-[", PreviousTab, Some("Workspace")),
        KeyBinding::new("secondary-shift-]", NextTab, Some("Workspace")),
        KeyBinding::new("secondary-{", PreviousTab, Some("Workspace")),
        KeyBinding::new("secondary-}", NextTab, Some("Workspace")),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some("Workspace")),
        KeyBinding::new("ctrl-tab", NextTab, Some("Workspace")),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some("Workspace")),
        KeyBinding::new("ctrl-pagedown", NextTab, Some("Workspace")),
        // ⌘1–⌘8 pick that tab and ⌘9 the last, like browsers.
        KeyBinding::new("secondary-1", SelectTab1, Some("Workspace")),
        KeyBinding::new("secondary-2", SelectTab2, Some("Workspace")),
        KeyBinding::new("secondary-3", SelectTab3, Some("Workspace")),
        KeyBinding::new("secondary-4", SelectTab4, Some("Workspace")),
        KeyBinding::new("secondary-5", SelectTab5, Some("Workspace")),
        KeyBinding::new("secondary-6", SelectTab6, Some("Workspace")),
        KeyBinding::new("secondary-7", SelectTab7, Some("Workspace")),
        KeyBinding::new("secondary-8", SelectTab8, Some("Workspace")),
        KeyBinding::new("secondary-9", LastTab, Some("Workspace")),
        // The macOS app's File and View menus.
        KeyBinding::new("shift-secondary-n", NewConnection, Some("Workspace")),
        KeyBinding::new("shift-secondary-e", EditConnection, Some("Workspace")),
        KeyBinding::new("shift-secondary-u", ShowUsers, Some("Workspace")),
        KeyBinding::new("secondary-b", ToggleSidebar, Some("Workspace")),
        // Arrow keys move through the connections, tables and roles lists, once one was clicked.
        KeyBinding::new("up", SelectPrevious, Some("ConnectionList")),
        KeyBinding::new("down", SelectNext, Some("ConnectionList")),
        KeyBinding::new("backspace", DeleteSelected, Some("ConnectionList")),
        KeyBinding::new("delete", DeleteSelected, Some("ConnectionList")),
        KeyBinding::new("up", SelectPrevious, Some("TableList")),
        KeyBinding::new("down", SelectNext, Some("TableList")),
        KeyBinding::new("up", SelectPrevious, Some("RoleList")),
        KeyBinding::new("down", SelectNext, Some("RoleList")),
    ]);
}

enum TabView {
    Table(Entity<TableTab>),
    Script(Entity<ScriptTab>),
    /// A connection's users (the role selected in the middle column).
    Users(Entity<UsersTab>),
    /// A script's results on their own.
    Result(Entity<ResultTab>),
}

impl TabView {
    fn entity_id(&self) -> EntityId {
        match self {
            TabView::Table(tab) => tab.entity_id(),
            TabView::Script(tab) => tab.entity_id(),
            TabView::Users(tab) => tab.entity_id(),
            TabView::Result(tab) => tab.entity_id(),
        }
    }

    fn has_unsaved_edits(&self, cx: &App) -> bool {
        match self {
            TabView::Table(tab) => tab.read(cx).has_unsaved_edits(cx),
            TabView::Script(tab) => tab.read(cx).has_unsaved_edits(cx),
            TabView::Result(tab) => tab.read(cx).has_unsaved_edits(cx),
            TabView::Users(_) => false,
        }
    }
}

struct OpenTab {
    /// The connection and database it uses.
    key: TargetKey,
    view: TabView,
    _events: Subscription,
}

impl OpenTab {
    fn title(&self, cx: &App) -> String {
        match &self.view {
            TabView::Table(tab) => tab.read(cx).title(),
            TabView::Script(tab) => tab.read(cx).name.clone(),
            TabView::Users(tab) => tab.read(cx).title(cx),
            TabView::Result(tab) => tab.read(cx).title.clone(),
        }
    }

    fn table(&self, cx: &App) -> Option<TableInfo> {
        match &self.view {
            TabView::Table(tab) => Some(tab.read(cx).table.clone()),
            TabView::Script(_) | TabView::Users(_) | TabView::Result(_) => None,
        }
    }

    fn filter(&self, cx: &App) -> Option<String> {
        match &self.view {
            TabView::Table(tab) => tab.read(cx).applied_filter(cx),
            TabView::Script(_) | TabView::Users(_) | TabView::Result(_) => None,
        }
    }
}

pub struct Workspace {
    /// Saved connections, passwords, open connections and their schemas, history and open tabs
    /// (`state.db`): shared with the CLI.
    service: Service,
    /// Reopening last session's tabs; nothing is saved until it's done.
    restoring: bool,
    restore_task: Option<Task<()>>,
    _quit: Option<Subscription>,
    _focus_lost: Subscription,
    _font_size: Subscription,
    /// Dumps and restores running in the background.
    backups: Entity<Backups>,
    _restored: Subscription,
    /// Enter in the open "New Database" dialog.
    _new_database: Option<Subscription>,
    connections: Vec<ConnectionConfig>,
    /// Why the saved connections or passwords aren't available, shown under the list.
    notice: Option<String>,
    /// No saved connections: the list shows the samples.
    showing_samples: bool,
    selected: Option<String>,
    /// The database shown for the selected connection.
    database: String,
    schemas: Load<Vec<Schema>>,
    collapsed: HashSet<String>,
    /// Collapsed connection groups (Local, Production…) in the sidebar.
    collapsed_groups: HashSet<String>,
    /// Connections whose databases are listed under them in the sidebar.
    expanded: HashSet<String>,
    /// Open connections the server closed (they reconnect when next used): no green light.
    lost: HashSet<String>,
    /// Connections whose last attempt failed: a warning instead of the light.
    failed: HashSet<String>,
    /// "New SQL Script" on a connection that wasn't open yet: opens once it is.
    script_after_connect: Option<TargetKey>,
    sidebar_visible: bool,
    /// The menu bar drawn in the window (Linux, Windows); macOS has its own.
    menu_bar: Option<Entity<gpui_kit::component::menu::AppMenuBar>>,
    connections_focus: FocusHandle,
    tables_focus: FocusHandle,
    roles_focus: FocusHandle,
    connections_scroll: ScrollHandle,
    tables_scroll: ScrollHandle,
    roles_scroll: ScrollHandle,
    /// The list whose selection the arrow keys just moved: scrolled to it on the next render.
    reveal: Cell<Option<List>>,
    _monitor: Task<()>,
    _inspector_shown: Subscription,
    /// The tables column hides views.
    tables_only: bool,
    /// The middle column lists users instead of tables (Postgres, MySQL).
    users_mode: bool,
    /// Each connection's users (roles are server-wide), and the tab showing them.
    users: HashMap<String, (Entity<UsersState>, Entity<UsersTab>)>,
    tabs: Vec<OpenTab>,
    active: usize,
    /// Scripts opened so far, for their names ("SQL 1", "SQL 2"…).
    scripts_made: usize,
    /// The tab bar's right-click menu while it's open, and where.
    tab_menu: Option<(Entity<PopupMenu>, Point<Pixels>)>,
    _tab_menu_dismiss: Option<Subscription>,
    focus: FocusHandle,
    // Dropping a task cancels it, so switching selection abandons the previous load.
    schemas_task: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The store (`DBEAR_CONNECTIONS_FILE` overrides it), the keychain (with the Cloudflare
        // sign-in write-back) and state.db. Without a store, the samples are listed.
        let service = Service::open_lenient(Options { samples: true, ..Default::default() });
        let mut notices = Vec::new();
        for problem in service.problems() {
            let e = &problem.error;
            match problem.part {
                Part::Store => notices.push(format!("Couldn’t open saved connections: {e}")),
                Part::Secrets => notices.push(format!("Saved passwords are unavailable ({e}).")),
                Part::State => log::warn!("couldn’t open the state file (no history or restored tabs): {e}"),
            }
        }
        let (connections, showing_samples) = service.listed_connections();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // When the focused element goes away (e.g. the grid, on switching to Structure), nothing would
        // have focus and the window's shortcuts (new script, close tab) would stop working.
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| window.focus(&this.focus, cx));
        // The script editor's text size, as last zoomed; saved whenever it changes.
        if let Some(size) = service.state_get(FONT_SIZE_KEY).and_then(|v| v.parse::<f32>().ok()) {
            cx.set_global(EditorFontSize(size));
        }
        let font_size = cx.observe_global::<EditorFontSize>(|this, cx| {
            let size = EditorFontSize::get(cx);
            if this.service.has_state() {
                let value = (size != EditorFontSize::DEFAULT).then(|| size.to_string());
                if let Err(e) = this.service.state_set(FONT_SIZE_KEY, value.as_deref()) {
                    log::warn!("couldn’t save the editor’s text size: {e}");
                }
            }
        });
        let backups = cx.new(|_| Backups::new(service.clone()));
        // A restore may have created or dropped tables: list them again.
        let restored = cx.subscribe(&backups, |this, _, event: &Restored, cx| {
            this.schema_changed(&event.0, cx);
        });
        // Script text isn't saved as you type: the last of it is saved when the app quits.
        let quit = cx.on_app_quit(|this, cx| {
            this.save_session(cx);
            async {}
        });
        crate::menus::install(MenuState { sidebar: true, inspector: crate::inspector::is_shown(cx) }, cx);
        let menu_bar = (!cfg!(target_os = "macos")).then(|| gpui_kit::component::menu::AppMenuBar::new(cx));
        let inspector_shown = cx.observe_global::<crate::inspector::ShowInspector>(|this, cx| this.refresh_menus(cx));
        let monitor = cx.spawn(async move |this, cx| Self::monitor(this, cx).await);
        Self {
            service,
            restoring: false,
            restore_task: None,
            _quit: Some(quit),
            _focus_lost: focus_lost,
            _font_size: font_size,
            backups,
            _restored: restored,
            _new_database: None,
            connections,
            showing_samples,
            notice: (!notices.is_empty()).then(|| notices.join("\n")),
            selected: None,
            database: String::new(),
            schemas: Load::Idle,
            collapsed: HashSet::new(),
            collapsed_groups: HashSet::new(),
            expanded: HashSet::new(),
            lost: HashSet::new(),
            failed: HashSet::new(),
            script_after_connect: None,
            sidebar_visible: true,
            menu_bar,
            connections_focus: cx.focus_handle(),
            tables_focus: cx.focus_handle(),
            roles_focus: cx.focus_handle(),
            connections_scroll: ScrollHandle::new(),
            tables_scroll: ScrollHandle::new(),
            roles_scroll: ScrollHandle::new(),
            reveal: Cell::new(None),
            _monitor: monitor,
            _inspector_shown: inspector_shown,
            tables_only: false,
            users_mode: false,
            users: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            scripts_made: 0,
            tab_menu: None,
            _tab_menu_dismiss: None,
            focus,
            schemas_task: None,
        }
    }

    /// Lists the saved connections again. When the first ones replace the samples, whatever was
    /// open on a sample (tabs, connections, the selection) goes with them.
    fn reload_connections(&mut self) {
        let (connections, showing_samples) = self.service.listed_connections();
        if self.showing_samples && !showing_samples {
            let samples: Vec<String> = self.connections.iter().map(|c| c.id.clone()).collect();
            for id in &samples {
                self.forget(id);
            }
            if self.selected.as_ref().is_some_and(|id| samples.contains(id)) {
                self.selected = None;
                self.schemas = Load::Idle;
                self.schemas_task = None;
            }
        }
        self.connections = connections;
        self.showing_samples = showing_samples;
    }

    /// Drops everything opened or cached for connection `id` (it was edited or deleted), its tabs
    /// too. Returns its connections, for closing.
    fn forget(&mut self, id: &str) -> Vec<Arc<Connection>> {
        self.tabs.retain(|tab| tab.key.connection_id != id);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.users.remove(id);
        self.expanded.remove(id);
        self.lost.remove(id);
        self.failed.remove(id);
        self.service.forget(id)
    }

    // MARK: editing connections

    fn open_editor(&mut self, original: Option<ConnectionConfig>, window: &mut Window, cx: &mut Context<Self>) {
        let title = if original.is_some() { "Edit Connection" } else { "New Connection" };
        let service = self.service.clone();
        let editor = cx.new(|cx| ConnectionEditor::new(original, service, window, cx));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (editor, workspace) = (editor.clone(), workspace.clone());
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(Button::new("test").outline().label("Test Connection").on_click({
                    let editor = editor.clone();
                    move |_, _, cx| editor.update(cx, |e, cx| e.test(cx))
                }))
                // The test result (or why saving failed), always in view below a long form.
                .child(match editor.read(cx).status_line(cx) {
                    Some(status) => status.into_any_element(),
                    None => div().flex_1().into_any_element(),
                })
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("save").primary().label("Save").on_click({
                    let editor = editor.clone();
                    move |_, window, cx| {
                        let saved = workspace.update(cx, |w, cx| w.save_connection(&editor, cx)).unwrap_or(false);
                        if saved {
                            window.close_dialog(cx);
                        }
                    }
                }));
            dialog.title(title).w(px(560.)).child(editor).footer(footer)
        });
    }

    /// Saves the editor's connection (and password). Returns whether it worked; errors show in the form.
    fn save_connection(&mut self, editor: &Entity<ConnectionEditor>, cx: &mut Context<Self>) -> bool {
        let crate::connection_editor::EditorValues { config, password, ssh_secret } = match editor.read(cx).values(cx) {
            Ok(values) => values,
            Err(e) => {
                editor.update(cx, |e2, cx| e2.show_error(e, cx));
                return false;
            }
        };
        let is_new = editor.read(cx).is_new();
        if !self.service.has_store() {
            editor.update(cx, |e, cx| e.show_error("The connection store isn’t available.".into(), cx));
            return false;
        }
        // The store, then the password and SSH secret (dropped with the tunnel, or for the agent).
        let dbservice::Saved { config: saved, problems } = match self.service.save(SaveRequest { config, password, ssh_secret }) {
            Ok(saved) => saved,
            Err(e) => {
                editor.update(cx, |ed, cx| ed.show_error(e.to_string(), cx));
                return false;
            }
        };
        for problem in problems {
            if problem.secret == SecretKind::Password && !self.service.has_secrets() {
                log::warn!("no keyring: the password for {} isn’t saved", saved.name);
            } else {
                editor.update(cx, |ed, cx| ed.show_error(problem.to_string(), cx));
            }
        }
        self.reload_connections();
        self.forget(&saved.id);
        // Reopen it with the new settings (or open the new one).
        if is_new || self.selected.as_deref() == Some(saved.id.as_str()) {
            self.selected = None;
            self.select_connection(saved.id, cx);
        }
        cx.notify();
        true
    }

    // MARK: connection actions

    /// Opens the connection (or tries again, when it's selected but failed).
    fn connect(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id.as_str()) {
            self.load_schemas_with(true, false, cx);
        } else {
            self.select_connection(id, cx);
        }
    }

    /// Closes the connection and its tabs; when it's selected, the window goes back to how it
    /// looks at launch (like the macOS app).
    fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        let open = self.forget(id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
            self.schemas = Load::Idle;
            self.schemas_task = None;
        }
        for connection in open {
            cx.background_executor().spawn(async move { connection.disconnect().await }).detach();
        }
        self.save_session(cx);
        cx.notify();
    }

    /// Closes every open connection (see [`Self::disconnect`]).
    fn disconnect_all(&mut self, cx: &mut Context<Self>) {
        let mut ids: Vec<String> = self.service.open_connections().into_iter().map(|(t, _)| t.connection_id).collect();
        ids.dedup();
        for id in ids {
            self.disconnect(&id, cx);
        }
    }

    /// Adds a copy named "… copy", with the same password, and selects it.
    fn duplicate(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        if !self.service.has_store() {
            return;
        }
        let service = self.service.clone();
        cx.spawn(async move |this, cx| {
            // The keyring can block (or prompt), so off the UI thread; selected once it's copied.
            let name = config.name.clone();
            let result = cx.background_executor().spawn(async move { service.duplicate(&config) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(saved) => {
                        for problem in &saved.problems {
                            log::warn!("the copy’s {} wasn’t saved: {}", problem.secret.label(), problem.error);
                        }
                        this.reload_connections();
                        this.select_connection(saved.config.id, cx);
                    }
                    Err(e) => this.notice = Some(format!("Couldn’t duplicate “{name}”: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Copies the connection's URL, its saved password included (like the macOS app).
    fn copy_url(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        let service = self.service.clone();
        cx.spawn(async move |_, cx| {
            // The keyring can block (or prompt): off the UI thread.
            match cx.background_executor().spawn(async move { service.url(&config.id, true) }).await {
                Ok(url) => {
                    cx.update(|cx| copy(url, cx));
                }
                Err(e) => log::warn!("couldn’t copy the URL: {e}"),
            }
        })
        .detach();
    }

    // MARK: new database

    fn open_new_database(&mut self, config: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| NewDatabaseDialog::new(config, window, cx));
        // The dialog takes focus as it opens: give it to the name once it's there.
        let name = dialog.read(cx).name_input();
        window.on_next_frame(move |window, cx| name.update(cx, |i, cx| i.focus(window, cx)));
        self._new_database = Some(cx.subscribe_in(&dialog, window, |this, dialog, _: &CreateRequested, window, cx| {
            this.create_database(dialog.clone(), window, cx)
        }));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |modal, _, cx| {
            let (ready, creating) = {
                let state = dialog.read(cx);
                (state.statement(cx).is_ok(), state.creating)
            };
            let (dialog, workspace) = (dialog.clone(), workspace.clone());
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1())
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(
                    Button::new("create")
                        .primary()
                        .label(if creating { "Creating…" } else { "Create" })
                        .disabled(!ready || creating)
                        .on_click({
                            let dialog = dialog.clone();
                            move |_, window, cx| {
                                let dialog = dialog.clone();
                                workspace.update(cx, |w, cx| w.create_database(dialog, window, cx)).ok();
                            }
                        }),
                );
            modal.title("New Database").w(px(480.)).child(dialog).footer(footer)
        });
    }

    fn create_database(&mut self, dialog: Entity<NewDatabaseDialog>, window: &mut Window, cx: &mut Context<Self>) {
        let (config, name) = {
            let state = dialog.read(cx);
            if state.creating || state.statement(cx).is_err() {
                return;
            }
            (state.config.clone(), state.name(cx))
        };
        dialog.update(cx, |d, cx| d.set_creating(true, cx));
        // Any open session on that server will do; else a short-lived one.
        let service = self.service.clone();
        cx.spawn_in(window, async move |this, cx| {
            match service.create_database(&config.id, &name).await {
                Ok(()) => {
                    this.update_in(cx, |w, window, cx| {
                        window.close_dialog(cx);
                        w.database_created(config, name, cx);
                    })
                    .ok();
                }
                Err(e) => {
                    dialog.update(cx, |d, cx| d.show_error(e.to_string(), cx));
                }
            }
        })
        .detach();
    }

    /// Lists the server's databases again (the service forgot them) and opens the new one.
    fn database_created(&mut self, config: ConnectionConfig, name: String, cx: &mut Context<Self>) {
        self._new_database = None;
        if self.selected.as_deref() != Some(config.id.as_str()) {
            self.select_connection(config.id.clone(), cx);
        }
        if dbservice::browses_databases(&config) {
            self.select_database(name, cx);
        } else {
            self.load_schemas_with(true, false, cx);
        }
    }

    // MARK: dump and restore

    /// The database a dump or restore of `config` starts on: the one shown when it's selected.
    fn backup_database(&self, config: &ConnectionConfig) -> (String, Vec<String>) {
        let database = if self.selected.as_deref() == Some(config.id.as_str()) {
            self.database.clone()
        } else {
            config.default_database().to_string()
        };
        let databases = if dbservice::browses_databases(config) { self.service.databases(&config.id).unwrap_or_default() } else { Vec::new() };
        (database, databases)
    }

    fn open_dump(&mut self, config: ConnectionConfig, preset: DumpPreset, window: &mut Window, cx: &mut Context<Self>) {
        let (database, databases) = self.backup_database(&config);
        let service = self.service.clone();
        let dialog = cx.new(|cx| DumpDialog::new(service, config, database, databases, preset, cx));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |modal, _, cx| {
            let ready = dialog.read(cx).can_dump();
            let title = format!("Dump “{}”", dbcore::dump::target_name(&dialog.read(cx).target_config()));
            let (dialog, workspace) = (dialog.clone(), workspace.clone());
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1())
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("dump").primary().label("Dump…").disabled(!ready).on_click({
                    let dialog = dialog.clone();
                    move |_, window, cx| {
                        let dialog = dialog.clone();
                        workspace.update(cx, |w, cx| w.start_dump(dialog, window, cx)).ok();
                    }
                }));
            modal.title(title).w(px(540.)).child(dialog).footer(footer)
        });
    }

    /// Asks where to save, then dumps in the background.
    fn start_dump(&mut self, dialog: Entity<DumpDialog>, window: &mut Window, cx: &mut Context<Self>) {
        let (target, options, name) = {
            let d = dialog.read(cx);
            (d.target(), d.options(), d.file_name())
        };
        let directory = dbcore::paths::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let path = cx.prompt_for_new_path(&directory, Some(&name));
        let backups = self.backups.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = path.await else { return };
            cx.update(|window, cx| {
                window.close_dialog(cx);
                backups.update(cx, |b, cx| b.dump(target, path, options, cx));
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a SQL file, then where to run it.
    fn open_restore(&mut self, config: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Restore".into()) });
        let (database, databases) = self.backup_database(&config);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(file) = paths.into_iter().next() else { return };
            this.update_in(cx, |w, window, cx| w.confirm_restore(RestoreDialog::new(config, database, databases, file), window, cx)).ok();
        })
        .detach();
    }

    fn confirm_restore(&mut self, restore: RestoreDialog, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|_| restore);
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |modal, _, cx| {
            let title = format!("Restore into “{}”", dbcore::dump::target_name(&dialog.read(cx).target_config()));
            let (dialog, workspace) = (dialog.clone(), workspace.clone());
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(div().flex_1())
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("restore").primary().label("Restore").on_click({
                    let dialog = dialog.clone();
                    move |_, window, cx| {
                        let (target, options, file) = {
                            let d = dialog.read(cx);
                            (d.target(), d.options(), d.file.clone())
                        };
                        window.close_dialog(cx);
                        workspace.update(cx, |w, cx| w.backups.update(cx, |b, cx| b.restore(target, file, options, cx))).ok();
                    }
                }));
            modal.title(title).w(px(500.)).child(dialog).footer(footer)
        });
    }

    // MARK: related rows

    /// Opens the rows a foreign key leads to, in a tab of their own after the current one.
    fn open_related(&mut self, key: TargetKey, related: RelatedRows, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.service.cached(&key) else { return };
        let table = self.service.table_info(&key, &related.schema, &related.table);
        let service = self.service.clone();
        // SQLite can reference a primary key without naming its columns: the service looks them up.
        cx.spawn_in(window, async move |this, cx| {
            let filter = match service.related_filter(&connection, &table, &related.columns, &related.values).await {
                Ok(filter) => filter,
                Err(e) => return log::warn!("couldn’t open the related rows: {e}"),
            };
            this.update_in(cx, |w, window, cx| w.open_filtered(key, connection, table, filter, window, cx)).ok();
        })
        .detach();
    }

    fn open_filtered(
        &mut self,
        key: TargetKey,
        connection: Arc<Connection>,
        table: TableInfo,
        filter: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = |t: &TableInfo| t.schema == table.schema && t.name == table.name;
        let existing = self.tabs.iter().position(|tab| {
            tab.key == key && tab.table(cx).as_ref().is_some_and(same) && tab.filter(cx).as_deref() == Some(filter.as_str())
        });
        if let Some(ix) = existing {
            return self.activate(ix, window, cx);
        }
        let view = TabView::Table(cx.new(|cx| {
            let mut tab = TableTab::new(connection, table, Some(filter), window, cx);
            tab.preview = false;
            tab
        }));
        let events = self.tab_events(&key, &view, window, cx);
        let ix = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(ix, OpenTab { key, view, _events: events });
        self.activate(ix, window, cx);
    }

    /// A script ran DDL on `key` (or a restore ended): its tables are listed again, in the
    /// background when shown.
    fn schema_changed(&mut self, key: &TargetKey, cx: &mut Context<Self>) {
        self.service.schema_changed(key);
        if self.target().is_some_and(|(_, current)| current == *key) {
            self.load_schemas_with(true, true, cx);
        }
    }

    // MARK: importing

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let service = self.service.clone();
        let import = cx.new(|cx| ImportDialog::new(service, cx));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (import, workspace) = (import.clone(), workspace.clone());
            let (has_connections, all_selected, count) = {
                let state = import.read(cx);
                (state.has_connections(), state.all_selected(), state.selected_count())
            };
            let footer = h_flex()
                .w_full()
                .gap_2()
                .child(Button::new("choose").outline().label("Choose…").tooltip("Another data-sources.json, or a DBeaver workspace folder").on_click({
                    let import = import.clone();
                    move |_, _, cx| import.update(cx, |d, cx| d.choose_source(cx))
                }))
                .when(has_connections, |footer| {
                    footer.child(Button::new("select-all").ghost().label(if all_selected { "Select None" } else { "Select All" }).on_click({
                        let import = import.clone();
                        move |_, _, cx| import.update(cx, |d, cx| d.toggle_all(cx))
                    }))
                })
                .child(div().flex_1())
                .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                .child(
                    Button::new("import")
                        .primary()
                        .label(if count > 0 { format!("Import {}", count) } else { "Import".to_string() })
                        .disabled(count == 0)
                        .on_click({
                            let import = import.clone();
                            move |_, window, cx| {
                            let done = workspace.update(cx, |w, cx| w.import_connections(&import, cx)).unwrap_or(false);
                            if done {
                                window.close_dialog(cx);
                            }
                        }}),
                );
            dialog.title("Import from DBeaver").w(px(600.)).child(import.clone()).footer(footer)
        });
    }

    /// Saves the dialog's checked connections, their passwords in the keyring. Returns whether to close it.
    fn import_connections(&mut self, dialog: &Entity<ImportDialog>, cx: &mut Context<Self>) -> bool {
        let configs = dialog.read(cx).selected_configs();
        if !self.service.has_store() {
            dialog.update(cx, |d, cx| d.show_error("The connection store isn’t available.".into(), cx));
            return false;
        }
        // Imported connections have no id yet, so each one is added (never replaces another).
        let dbservice::Import { imported, problems } = match self.service.import(configs) {
            Ok(import) => import,
            Err(e) => {
                dialog.update(cx, |d, cx| d.show_error(e.to_string(), cx));
                return false;
            }
        };
        let imported = imported.len();
        if imported == 0 {
            dialog.update(cx, |d, cx| d.show_error(problems.join("\n"), cx));
            return false;
        }
        self.reload_connections();
        // Nothing is opened: imported connections often point at servers that shouldn't be hit unasked.
        self.notice = (!problems.is_empty()).then(|| format!("Imported {imported}, with problems:\n{}", problems.join("\n")));
        self.save_session(cx);
        cx.notify();
        true
    }

    fn confirm_delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.connections.iter().find(|c| c.id == id) else { return };
        let name = config.name.clone();
        let workspace = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, id) = (workspace.clone(), id.clone());
            alert
                .title(format!("Delete “{name}”?"))
                .description("Its saved password is deleted too. This can’t be undone.")
                .show_cancel(true)
                .button_props(DialogButtonProps::default().ok_text("Delete"))
                .on_ok(move |_, _, cx| {
                    workspace.update(cx, |w, cx| w.delete_connection(&id, cx)).ok();
                    true
                })
        });
    }

    fn delete_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.service.has_store() {
            return;
        }
        // The store, then its passwords and history.
        if let Err(e) = self.service.delete(id) {
            log::error!("couldn’t delete the connection: {e}");
            return;
        }
        self.forget(id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
            self.schemas = Load::Idle;
            self.schemas_task = None;
        }
        self.reload_connections();
        cx.notify();
    }

    fn selected_connection(&self) -> Option<&ConnectionConfig> {
        let id = self.selected.as_deref()?;
        self.connections.iter().find(|c| c.id == id)
    }

    fn select_connection(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        let Some(config) = self.connections.iter().find(|c| c.id == id) else { return };
        let database = self.service.target_for(config, None).database;
        self.selected = Some(id);
        self.database = database;
        self.collapsed.clear();
        self.load_schemas(cx);
        self.save_session(cx);
    }

    fn select_database(&mut self, database: String, cx: &mut Context<Self>) {
        let Some(config) = self.selected_connection().cloned() else { return };
        if database == self.database {
            return;
        }
        // Kept in the connection store (shared with the macOS app).
        if let Err(e) = self.service.set_last_database(&config.id, &database) {
            log::warn!("couldn’t remember the database: {e}");
        }
        self.database = database;
        self.collapsed.clear();
        self.load_schemas(cx);
        self.save_session(cx);
    }

    fn target(&self) -> Option<(ConnectionConfig, TargetKey)> {
        let config = self.selected_connection()?;
        Some((dbservice::pointed_at(config, &self.database), TargetKey::new(config.id.clone(), self.database.clone())))
    }

    fn load_schemas(&mut self, cx: &mut Context<Self>) {
        self.load_schemas_with(false, false, cx);
    }

    /// Lists the selected target's tables. `refresh`: ignore what's cached (and list the databases
    /// again too); `quiet`: keep showing the current list until the new one arrives.
    fn load_schemas_with(&mut self, refresh: bool, quiet: bool, cx: &mut Context<Self>) {
        let Some((_, key)) = self.target() else { return };
        if refresh {
            self.service.schema_changed(&key);
        } else if let Some(schemas) = self.service.cached_schemas(&key) {
            self.schemas = Load::Loaded(schemas);
            self.schemas_task = None;
            cx.notify();
            return;
        }
        if !(quiet && matches!(self.schemas, Load::Loaded(_))) {
            self.schemas = Load::Loading;
        }
        let service = self.service.clone();
        self.schemas_task = Some(cx.spawn(async move |this, cx| {
            // Reuses the target's connection, or opens one (secrets read off the UI thread).
            let result = service.load(&key, refresh).await;
            this.update(cx, |this, cx| {
                let current = this.target().map(|(_, k)| k);
                if current.as_ref() != Some(&key) {
                    return;
                }
                match result {
                    Ok(loaded) => {
                        this.failed.remove(&key.connection_id);
                        this.schemas = Load::Loaded(loaded.schemas);
                        // The remembered database was dropped or renamed: back to the connection's own.
                        if loaded.database_gone {
                            if let Some(default) = this.selected_connection().map(|c| c.default_database().to_string()) {
                                this.database = default;
                                this.load_schemas(cx);
                            }
                        }
                    }
                    // A background refresh: keep the list it couldn't replace.
                    Err(e) if quiet && matches!(this.schemas, Load::Loaded(_)) => log::warn!("couldn’t list the tables again: {e}"),
                    Err(e) => {
                        this.failed.insert(key.connection_id.clone());
                        this.schemas = Load::Failed(e.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // MARK: tabs

    fn open_connection(&self) -> Option<(TargetKey, Arc<Connection>)> {
        let (_, key) = self.target()?;
        let connection = self.service.cached(&key)?;
        Some((key, connection))
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = ix;
        if let TabView::Script(script) = &self.tabs[ix].view {
            let focus = script.read(cx).editor_focus(cx);
            focus.focus(window, cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    /// Opens `table` in a tab. A single click reuses the preview tab; `pin` (double-click) keeps it.
    fn open_table(&mut self, table: TableInfo, pin: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, connection)) = self.open_connection() else { return };
        let existing = self.tabs.iter().position(|tab| {
            tab.key == key && tab.table(cx).as_ref() == Some(&table) && tab.filter(cx).is_none()
        });
        if let Some(ix) = existing {
            if let (true, TabView::Table(tab)) = (pin, &self.tabs[ix].view) {
                tab.update(cx, |tab, cx| {
                    tab.preview = false;
                    cx.notify();
                });
            }
            self.activate(ix, window, cx);
            return;
        }
        let view = TabView::Table(cx.new(|cx| {
            let mut tab = TableTab::new(connection, table, None, window, cx);
            tab.preview = !pin;
            tab
        }));
        let events = self.tab_events(&key, &view, window, cx);
        let tab = OpenTab { key, view, _events: events };
        let preview = self.tabs.iter().position(|tab| match &tab.view {
            TabView::Table(t) => t.read(cx).preview,
            TabView::Script(_) | TabView::Users(_) | TabView::Result(_) => false,
        });
        match preview {
            Some(ix) => self.tabs[ix] = tab,
            None => self.tabs.push(tab),
        }
        let ix = preview.unwrap_or(self.tabs.len() - 1);
        self.activate(ix, window, cx);
    }

    fn new_script(&mut self, _: &NewScript, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, connection)) = self.open_connection() else { return };
        self.scripts_made += 1;
        let name = format!("SQL {}", self.scripts_made);
        let (label, runs) = (self.target_label(&key), self.script_target(&key));
        let view = TabView::Script(cx.new(|cx| ScriptTab::new(connection, name, label, runs, window, cx)));
        let events = self.tab_events(&key, &view, window, cx);
        self.tabs.push(OpenTab { key, view, _events: events });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    /// Handles what a tab on `key` asks for (related rows, refreshing after DDL).
    fn tab_events(&self, key: &TargetKey, view: &TabView, window: &Window, cx: &mut Context<Self>) -> Subscription {
        let key = key.clone();
        let handle = move |this: &mut Self, event: &TabEvent, window: &mut Window, cx: &mut Context<Self>| match event {
            TabEvent::OpenRelated(related) => this.open_related(key.clone(), related.clone(), window, cx),
            TabEvent::SchemaMayHaveChanged => this.schema_changed(&key, cx),
            TabEvent::OpenResults(request) => this.open_results(key.clone(), request.clone(), window, cx),
        };
        match view {
            TabView::Table(tab) => {
                let handle = handle.clone();
                cx.subscribe_in(tab, window, move |this, _, event, window, cx| handle(this, event, window, cx))
            }
            TabView::Script(tab) => {
                let handle = handle.clone();
                cx.subscribe_in(tab, window, move |this, _, event, window, cx| handle(this, event, window, cx))
            }
            TabView::Result(tab) => cx.subscribe_in(tab, window, move |this, _, event, window, cx| handle(this, event, window, cx)),
            // Its title follows the selected role.
            TabView::Users(tab) => cx.observe(tab, |_, _, cx| cx.notify()),
        }
    }

    /// Opens a script's results in a tab after the current one ("SQL 1 Results", then "… 2").
    fn open_results(&mut self, key: TargetKey, request: OpenResults, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.service.cached(&key) else { return };
        let base = format!("{} Results", request.source);
        let taken: HashSet<String> = self.tabs.iter().map(|t| t.title(cx)).collect();
        let title = std::iter::once(base.clone())
            .chain((2..).map(|n| format!("{base} {n}")))
            .find(|t| !taken.contains(t))
            .unwrap_or(base);
        let (label, runs) = (self.target_label(&key), self.script_target(&key));
        let view = TabView::Result(cx.new(|cx| ResultTab::new(connection, title, label, request, runs, window, cx)));
        let events = self.tab_events(&key, &view, window, cx);
        let ix = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(ix, OpenTab { key, view, _events: events });
        self.activate(ix, window, cx);
    }

    /// Where a script on `key` runs (and records its history).
    fn script_target(&self, key: &TargetKey) -> ScriptTarget {
        ScriptTarget { service: self.service.clone(), target: key.clone() }
    }

    /// "conn · db" (or just the connection's name) for a script's header.
    fn target_label(&self, key: &TargetKey) -> String {
        self.service.target_label(key)
    }

    // MARK: session

    fn save_session(&self, cx: &App) {
        if !self.service.has_state() || self.restoring {
            return;
        }
        let tabs = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let TargetKey { connection_id: connection, database } = tab.key.clone();
                Some(match &tab.view {
                    // Users tabs aren't reopened: the middle column is back to tables at launch.
                    TabView::Users(_) => return None,
                    // Nor results: running a script by itself at launch could write.
                    TabView::Result(_) => return None,
                    TabView::Table(t) => {
                        let tab = t.read(cx);
                        let table = &tab.table;
                        SavedTab::Table {
                            connection,
                            database,
                            schema: table.schema.clone(),
                            table: table.name.clone(),
                            view: table.kind == TableKind::View,
                            filter: tab.applied_filter(cx),
                        }
                    }
                    TabView::Script(s) => {
                        let s = s.read(cx);
                        SavedTab::Script { connection, database, name: s.name.clone(), sql: s.text(cx) }
                    }
                })
            })
            .collect();
        let session = Session {
            selected: self.selected.clone().map(|id| (id, self.database.clone())),
            active: self.active,
            tabs,
        };
        match serde_json::to_string(&session) {
            Ok(json) => {
                if let Err(e) = self.service.state_set(SESSION_KEY, Some(&json)) {
                    log::warn!("couldn’t save the open tabs: {e}");
                }
            }
            Err(e) => log::warn!("couldn’t save the open tabs: {e}"),
        }
    }

    /// Reopens last session's selection and tabs (tabs whose connection fails are skipped).
    pub fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(json) = self.service.state_get(SESSION_KEY) else { return };
        let session: Session = match serde_json::from_str(&json) {
            Ok(session) => session,
            Err(e) => return log::warn!("ignoring the saved tabs: {e}"),
        };
        if let Some((id, database)) = session.selected.filter(|(id, _)| self.connections.iter().any(|c| c.id == *id)) {
            self.select_connection(id, cx);
            if !database.is_empty() && database != self.database {
                self.select_database(database, cx);
            }
        }
        if session.tabs.is_empty() {
            return;
        }
        self.restoring = true;
        let service = self.service.clone();
        self.restore_task = Some(cx.spawn_in(window, async move |this, cx| {
            for saved in session.tabs {
                let key = match &saved {
                    SavedTab::Table { connection, database, .. } | SavedTab::Script { connection, database, .. } => {
                        TargetKey::new(connection.clone(), database.clone())
                    }
                };
                let listed = this.update(cx, |w, _| w.connections.iter().any(|c| c.id == key.connection_id)).unwrap_or(false);
                if !listed {
                    continue;
                }
                // Connects now; a target that fails isn't kept (its tabs are skipped).
                let connection = match service.connected(&key).await {
                    Ok(connection) => connection,
                    Err(e) => {
                        log::warn!("not reopening a tab on {}: {e}", key.connection_id);
                        continue;
                    }
                };
                this.update_in(cx, |w, window, cx| w.push_restored(key, saved, connection, window, cx)).ok();
            }
            this.update_in(cx, |w, window, cx| {
                w.restoring = false;
                let active = session.active.min(w.tabs.len().saturating_sub(1));
                w.activate(active, window, cx);
            })
            .ok();
        }));
    }

    fn push_restored(&mut self, key: TargetKey, saved: SavedTab, connection: Arc<Connection>, window: &mut Window, cx: &mut Context<Self>) {
        let view = match saved {
            SavedTab::Table { schema, table, view, filter, .. } => {
                let mut table = TableInfo::new(schema, table);
                if view {
                    table.kind = TableKind::View;
                }
                TabView::Table(cx.new(|cx| {
                    let mut tab = TableTab::new(connection, table, filter, window, cx);
                    tab.preview = false;
                    tab
                }))
            }
            SavedTab::Script { name, sql, .. } => {
                // Keep numbering new scripts after the restored ones.
                if let Some(n) = name.strip_prefix("SQL ").and_then(|n| n.parse::<usize>().ok()) {
                    self.scripts_made = self.scripts_made.max(n);
                }
                let (label, runs) = (self.target_label(&key), self.script_target(&key));
                TabView::Script(cx.new(|cx| {
                    let mut tab = ScriptTab::new(connection, name, label, runs, window, cx);
                    tab.set_text(&sql, window, cx);
                    tab
                }))
            }
        };
        let events = self.tab_events(&key, &view, window, cx);
        self.tabs.push(OpenTab { key, view, _events: events });
        cx.notify();
    }

    fn close_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = match self.tabs.get(ix).map(|t| &t.view) {
            Some(view) => view.has_unsaved_edits(cx),
            None => return,
        };
        if unsaved {
            let workspace = cx.entity().downgrade();
            let title = self.tabs[ix].title(cx);
            window.open_alert_dialog(cx, move |alert, _, _| {
                let workspace = workspace.clone();
                alert
                    .title(format!("Discard unsaved changes to “{title}”?"))
                    .description("Your edits haven’t been saved.")
                    .show_cancel(true)
                    .button_props(DialogButtonProps::default().ok_text("Discard"))
                    .on_ok(move |_, window, cx| {
                        workspace.update(cx, |w, cx| w.remove_tab(ix, window, cx)).ok();
                        true
                    })
            });
            return;
        }
        self.remove_tab(ix, window, cx);
    }

    fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.tabs.is_empty() {
            window.focus(&self.focus, cx);
            self.save_session(cx);
        } else {
            self.activate(self.active, window, cx);
        }
        cx.notify();
    }

    /// Closes every tab in `close` (indices), asking once if any of them has unsaved edits.
    /// `keep` is the tab to show afterwards when it's still open.
    fn close_tabs(&mut self, close: Vec<usize>, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = close
            .iter()
            .filter(|&&ix| self.tabs.get(ix).is_some_and(|t| t.view.has_unsaved_edits(cx)))
            .count();
        if unsaved == 0 {
            return self.remove_tabs(close, keep, window, cx);
        }
        let workspace = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, close) = (workspace.clone(), close.clone());
            alert
                .title(if unsaved == 1 {
                    "Discard unsaved changes in 1 tab?".to_string()
                } else {
                    format!("Discard unsaved changes in {unsaved} tabs?")
                })
                .description("Your edits haven’t been saved.")
                .show_cancel(true)
                .button_props(DialogButtonProps::default().ok_text("Discard"))
                .on_ok(move |_, window, cx| {
                    workspace.update(cx, |w, cx| w.remove_tabs(close.clone(), keep, window, cx)).ok();
                    true
                })
        });
    }

    fn remove_tabs(&mut self, mut close: Vec<usize>, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        close.sort_unstable();
        close.dedup();
        let shown = if close.contains(&self.active) { keep } else { self.active };
        let before = |ix: usize| close.iter().filter(|&&c| c < ix).count();
        let shown = (!close.contains(&shown)).then(|| shown - before(shown));
        for &ix in close.iter().rev() {
            if ix < self.tabs.len() {
                self.tabs.remove(ix);
            }
        }
        if self.tabs.is_empty() {
            self.active = 0;
            window.focus(&self.focus, cx);
            self.save_session(cx);
        } else {
            let ix = shown.unwrap_or(self.active.saturating_sub(before(self.active))).min(self.tabs.len() - 1);
            self.activate(ix, window, cx);
        }
        cx.notify();
    }

    /// Keeps a preview tab open (like double-clicking its table).
    fn keep_open(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(TabView::Table(tab)) = self.tabs.get(ix).map(|t| &t.view) {
            tab.update(cx, |tab, cx| {
                tab.preview = false;
                cx.notify();
            });
            self.save_session(cx);
            cx.notify();
        }
    }

    /// The right-click menu of the tab at `ix`, opened at `position`.
    fn open_tab_menu(&mut self, ix: usize, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len();
        let preview = matches!(self.tabs.get(ix).map(|t| &t.view), Some(TabView::Table(t)) if t.read(cx).preview);
        let this = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            // A menu item that runs `$body` on the workspace.
            macro_rules! act {
                ($label:expr, |$w:ident, $window:ident, $cx:ident| $body:expr) => {{
                    let this = this.clone();
                    PopupMenuItem::new($label).on_click(move |_, $window, cx| {
                        this.update(cx, |$w, $cx| $body).ok();
                    })
                }};
            }
            menu.when(preview, |menu| menu.item(act!("Keep Open", |w, _window, cx| w.keep_open(ix, cx))).separator())
                .item(act!("Close Tab", |w, window, cx| w.close_tab_at(ix, window, cx)))
                .item(
                    act!("Close Other Tabs", |w, window, cx| {
                        let others = (0..w.tabs.len()).filter(|&i| i != ix).collect();
                        w.close_tabs(others, ix, window, cx)
                    })
                    .disabled(count < 2),
                )
                .item(
                    act!("Close Tabs to the Right", |w, window, cx| {
                        let right = (ix + 1..w.tabs.len()).collect();
                        w.close_tabs(right, ix, window, cx)
                    })
                    .disabled(ix + 1 >= count),
                )
                .separator()
                .item(act!("Close All Tabs", |w, window, cx| {
                    let all = (0..w.tabs.len()).collect();
                    w.close_tabs(all, 0, window, cx)
                }))
        });
        self._tab_menu_dismiss = Some(cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
            this.tab_menu = None;
            cx.notify();
        }));
        menu.read(cx).focus_handle(cx).focus(window, cx);
        self.tab_menu = Some((menu, position));
        cx.notify();
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            window.remove_window();
        } else {
            self.close_tab_at(self.active, window, cx);
        }
    }

    /// Moves through the tabs by `offset`, wrapping around.
    fn cycle_tab(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len() as isize;
        if count > 1 {
            let ix = (self.active as isize + offset).rem_euclid(count) as usize;
            self.activate(ix, window, cx);
        }
    }

    fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.activate(ix, window, cx);
        }
    }

    /// Drops the tab at `from` at `to`, keeping the same tab active.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = if self.active == from {
            to
        } else if from < self.active && to >= self.active {
            self.active - 1
        } else if from > self.active && to <= self.active {
            self.active + 1
        } else {
            self.active
        };
        self.save_session(cx);
        cx.notify();
    }

    // MARK: menus, sidebar and keyboard

    /// Sets the menus again (what they show checked changed).
    fn refresh_menus(&self, cx: &mut Context<Self>) {
        crate::menus::install(MenuState { sidebar: self.sidebar_visible, inspector: crate::inspector::is_shown(cx) }, cx);
        if let Some(bar) = &self.menu_bar {
            bar.update(cx, |bar, cx| bar.reload(cx));
        }
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        self.refresh_menus(cx);
        cx.notify();
    }

    /// Asks the open connections, every few seconds, whether the server still has them, so the
    /// green light goes out when it closed one (like the macOS app). They reconnect when next used.
    async fn monitor(this: WeakEntity<Self>, cx: &mut AsyncApp) {
        loop {
            cx.background_executor().timer(MONITOR_INTERVAL).await;
            let Ok(service) = this.update(cx, |w, _| w.service.clone()) else { return };
            let lost = service.lost_connections().await;
            let updated = this.update(cx, |w, cx| {
                if w.lost != lost {
                    w.lost = lost;
                    cx.notify();
                }
            });
            if updated.is_err() {
                return;
            }
        }
    }

    // MARK: command palette

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        let focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let entries = self.palette_entries(&focus, window, cx);
        Palette::open(entries, cx.entity(), window, cx);
    }

    /// Everything the palette lists, in the order an empty query shows it: commands, open tabs,
    /// connections (and their databases), then the tables of every connection listed so far,
    /// the shown one's first.
    /// `focus`: what had focus before the palette, which its commands go to.
    pub(crate) fn palette_entries(&self, focus: &FocusHandle, window: &Window, cx: &App) -> Vec<palette::Entry> {
        let menu_state = MenuState { sidebar: self.sidebar_visible, inspector: crate::inspector::is_shown(cx) };
        let mut entries = Vec::new();
        // Commands that nothing where focus is handles are left out, as the menus disable them, and
        // so are the window's that wouldn't do anything now.
        for command in crate::menus::palette_commands(menu_state) {
            let action = command.action.as_ref();
            if !command.global && !window.is_action_available_in(action, focus) || !self.command_applies(action, cx) {
                continue;
            }
            let shortcut = window
                .highest_precedence_binding_for_action_in(action, focus)
                .or_else(|| window.highest_precedence_binding_for_action_in_context(action, KeyContext::default()))
                .and_then(|b| b.keystrokes().first().map(|k| k.as_keystroke().clone()));
            entries.push(palette::Entry {
                candidate: Candidate::new(Kind::Command, command.label).keywords(command.keywords.iter().copied()),
                detail: None,
                icon: Icon::new(AssetIcon::Command),
                shortcut,
                target: Target::Action(command.action),
            });
        }
        for (ix, tab) in self.tabs.iter().enumerate() {
            let target = self.target_label(&tab.key);
            let icon = match &tab.view {
                TabView::Table(t) if t.read(cx).table.kind == TableKind::View => Icon::new(IconName::Eye),
                TabView::Table(_) => Icon::new(IconName::FileText),
                TabView::Script(_) | TabView::Result(_) => Icon::new(IconName::SquareTerminal),
                TabView::Users(_) => Icon::new(AssetIcon::Users),
            };
            let detail = if ix == self.active { format!("{target} \u{b7} current tab") } else { format!("{target} \u{b7} tab") };
            entries.push(palette::Entry {
                candidate: Candidate::new(Kind::Tab, tab.title(cx)).keywords([target, "tab".into()]),
                detail: Some(detail.into()),
                icon,
                shortcut: None,
                target: Target::Tab(tab.view.entity_id()),
            });
        }
        for c in &self.connections {
            let mut keywords = vec![c.group.clone(), c.host.clone(), c.database.clone(), c.kind.display_name().to_string()];
            // Not a file's path (SQLite): its folders would match almost anything.
            keywords.retain(|k| !k.is_empty() && !k.contains(['/', '\\']));
            let detail = if c.group.is_empty() { c.kind.display_name().to_string() } else { format!("{} \u{b7} {}", c.group, c.kind.display_name()) };
            entries.push(palette::Entry {
                candidate: Candidate::new(Kind::Connection, c.name.clone()).keywords(keywords),
                detail: Some(detail.into()),
                icon: crate::assets::kind_icon(c.kind),
                shortcut: None,
                target: Target::Connection { id: c.id.clone(), database: None },
            });
            for database in self.sidebar_databases(c).into_iter().flatten() {
                entries.push(palette::Entry {
                    candidate: Candidate::new(Kind::Connection, database.clone()).keywords([c.name.clone(), "database".into()]),
                    detail: Some(format!("database on {}", c.name).into()),
                    icon: Icon::new(AssetIcon::Database),
                    shortcut: None,
                    target: Target::Connection { id: c.id.clone(), database: Some(database.clone()) },
                });
            }
        }
        let current = self.target().map(|(_, key)| key);
        let mut listed = self.service.schema_targets();
        listed.sort_by_key(|key| (Some(key) != current.as_ref(), self.target_label(key)));
        for key in &listed {
            entries.extend(self.table_entries(key));
        }
        entries
    }

    /// The palette's entries for a target's tables (none until they're listed).
    fn table_entries(&self, key: &TargetKey) -> Vec<palette::Entry> {
        let target = self.target_label(key);
        let mut entries = Vec::new();
        for schema in self.service.cached_schemas(key).iter().flatten() {
            for table in &schema.tables {
                let qualified = if schema.name.is_empty() { table.name.clone() } else { format!("{}.{}", schema.name, table.name) };
                let detail = if schema.name.is_empty() { target.clone() } else { format!("{} \u{b7} {target}", schema.name) };
                let view = table.kind == TableKind::View;
                entries.push(palette::Entry {
                    candidate: Candidate::new(Kind::Table, table.name.clone()).keywords([qualified, target.clone()]),
                    detail: Some(detail.into()),
                    icon: Icon::new(if view { IconName::Eye } else { IconName::FileText }),
                    shortcut: None,
                    target: Target::Table { connection: key.connection_id.clone(), database: key.database.clone(), table: table.clone() },
                });
            }
        }
        entries
    }

    /// Tab on a connection in the palette: selects (connects to) it; returns what it shows and its name.
    pub(crate) fn palette_enter(&mut self, id: String, database: Option<String>, cx: &mut Context<Self>) -> Option<(TargetKey, String)> {
        self.palette_chose_connection(id, database, cx);
        let (config, key) = self.target()?;
        // The database, as in "app_dev > orders"; a file's path (SQLite) reads better as the connection.
        let database = config.default_database();
        let name = if database.is_empty() || database.contains(['/', '\\']) { config.name.clone() } else { database.to_string() };
        Some((key, name))
    }

    /// The tables of `key` for the palette, once listed.
    pub(crate) fn palette_tables(&self, key: &TargetKey, _: &App) -> (PaletteTables, Vec<palette::Entry>) {
        if self.service.cached_schemas(key).is_some() {
            return (PaletteTables::Ready, self.table_entries(key));
        }
        let shown = self.target().is_some_and(|(_, k)| &k == key);
        match &self.schemas {
            Load::Failed(e) if shown => (PaletteTables::Failed(e.clone()), Vec::new()),
            // Moved on to another connection meanwhile: its tables won't come.
            _ if !shown => (PaletteTables::Failed("No longer selected".into()), Vec::new()),
            _ => (PaletteTables::Loading, Vec::new()),
        }
    }

    fn palette_chose_connection(&mut self, id: String, database: Option<String>, cx: &mut Context<Self>) {
        self.users_mode = false;
        if database.is_some() {
            self.expanded.insert(id.clone());
        }
        self.select_row((id, database), cx);
    }

    /// Whether one of the window's commands would do something now.
    fn command_applies(&self, action: &dyn Action, cx: &App) -> bool {
        let any = action.as_any();
        let table_tab = matches!(self.tabs.get(self.active).map(|t| &t.view), Some(TabView::Table(_)));
        if any.is::<NewScript>() {
            self.open_connection().is_some()
        } else if any.is::<EditConnection>() {
            self.selected_connection().is_some()
        } else if any.is::<Disconnect>() {
            self.selected.as_deref().is_some_and(|id| self.is_open(id))
        } else if any.is::<DisconnectAll>() {
            !self.service.open_connections().is_empty()
        } else if any.is::<ShowUsers>() {
            self.selected_connection().is_some_and(|c| dbcore::access::features(c.kind).is_some())
        } else if any.is::<ShowData>() || any.is::<ShowStructure>() || any.is::<AddRow>() {
            table_tab && self.active_table(cx).is_some()
        } else if any.is::<CloseTab>() {
            !self.tabs.is_empty()
        } else if any.is::<PreviousTab>() || any.is::<NextTab>() {
            self.tabs.len() > 1
        } else {
            true
        }
    }

    pub(crate) fn palette_chose(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Tab(id) => {
                if let Some(ix) = self.tabs.iter().position(|tab| tab.view.entity_id() == id) {
                    self.activate(ix, window, cx);
                }
            }
            Target::Connection { id, database } => self.palette_chose_connection(id, database, cx),
            Target::Table { connection, database, table } => {
                self.users_mode = false;
                self.select_connection(connection, cx);
                if self.database != database {
                    self.select_database(database, cx);
                }
                self.open_table(table, true, window, cx);
            }
            // Sent by the palette itself.
            Target::Action(_) => {}
        }
        cx.notify();
    }

    /// Opens a script on a connection (and database), connecting first when needed.
    fn new_script_on(&mut self, id: String, database: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.select_connection(id, cx);
        if let Some(database) = database {
            self.select_database(database, cx);
        }
        if self.open_connection().is_some() {
            self.new_script(&NewScript, window, cx);
        } else {
            self.script_after_connect = self.target().map(|(_, key)| key);
        }
    }

    /// The pending "New SQL Script", once its connection is open (or dropped when it failed).
    fn open_pending_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.script_after_connect.clone() else { return };
        if self.target().map(|(_, k)| k).as_ref() != Some(&key) || matches!(self.schemas, Load::Failed(_)) {
            self.script_after_connect = None;
        } else if self.service.cached(&key).is_some() {
            self.script_after_connect = None;
            self.new_script(&NewScript, window, cx);
        }
    }

    /// The databases listed under `config` in the sidebar: once listed, and only when there's a choice.
    fn sidebar_databases(&self, config: &ConnectionConfig) -> Option<Vec<String>> {
        self.service.databases(&config.id).filter(|d| d.len() > 1 && dbservice::browses_databases(config))
    }

    /// Whether connection `id` has an open session (its status light).
    fn is_open(&self, id: &str) -> bool {
        self.service.open_connections().iter().any(|(target, _)| target.connection_id == id)
    }

    fn shows_databases(&self, config: &ConnectionConfig) -> bool {
        self.expanded.contains(&config.id) && self.sidebar_databases(config).is_some()
    }

    /// The highlighted sidebar row: the shown database's while its connection is expanded.
    fn selected_row(&self) -> Option<SidebarRow> {
        let config = self.selected_connection()?;
        let database = self.sidebar_databases(config).filter(|d| self.shows_databases(config) && d.contains(&self.database));
        Some((config.id.clone(), database.map(|_| self.database.clone())))
    }

    /// The connections in sidebar order, by group (empty group: "Connections").
    fn grouped_connections(&self) -> Vec<(String, Vec<&ConnectionConfig>)> {
        let mut groups: Vec<(String, Vec<&ConnectionConfig>)> = Vec::new();
        for c in &self.connections {
            match groups.iter_mut().find(|(g, _)| *g == c.group) {
                Some((_, list)) => list.push(c),
                None => groups.push((c.group.clone(), vec![c])),
            }
        }
        groups
    }

    fn visible_sidebar_rows(&self) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        for (group, connections) in self.grouped_connections() {
            if self.collapsed_groups.contains(&group) {
                continue;
            }
            for c in connections {
                rows.push((c.id.clone(), None));
                if self.shows_databases(c) {
                    rows.extend(self.sidebar_databases(c).into_iter().flatten().map(|d| (c.id.clone(), Some(d.clone()))));
                }
            }
        }
        rows
    }

    fn select_row(&mut self, (id, database): SidebarRow, cx: &mut Context<Self>) {
        self.select_connection(id, cx);
        if let Some(database) = database {
            self.select_database(database, cx);
        }
        cx.notify();
    }

    /// The tables the tables column shows, in order.
    fn visible_tables(&self) -> Vec<TableInfo> {
        let Load::Loaded(schemas) = &self.schemas else { return Vec::new() };
        schemas
            .iter()
            .filter(|s| !self.collapsed.contains(&s.name))
            .flat_map(|s| s.tables.iter().filter(|t| !self.tables_only || t.kind == TableKind::Table))
            .cloned()
            .collect()
    }

    /// The table of the active tab, when it's on the shown connection and database.
    fn active_table(&self, cx: &App) -> Option<TableInfo> {
        self.tabs.get(self.active).filter(|tab| self.target().is_some_and(|(_, key)| key == tab.key)).and_then(|tab| tab.table(cx))
    }

    /// ↑ / ↓ in the focused list.
    fn move_selection(&mut self, list: List, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        fn step<T: PartialEq + Clone>(items: &[T], current: Option<&T>, offset: isize) -> Option<T> {
            let ix = match current.and_then(|c| items.iter().position(|i| i == c)) {
                Some(ix) => ix.checked_add_signed(offset).filter(|&ix| ix < items.len())?,
                None if offset > 0 => 0,
                None => items.len().checked_sub(1)?,
            };
            items.get(ix).cloned()
        }
        match list {
            List::Connections => {
                let rows = self.visible_sidebar_rows();
                let Some(next) = step(&rows, self.selected_row().as_ref(), offset) else { return };
                self.select_row(next, cx);
            }
            List::Tables => {
                let tables = self.visible_tables();
                let Some(next) = step(&tables, self.active_table(cx).as_ref(), offset) else { return };
                self.open_table(next, false, window, cx);
                // Opening the tab focuses its grid: the arrows stay with the list.
                self.tables_focus.focus(window, cx);
            }
            List::Roles => {
                let Some(state) = self.selected_connection().and_then(|c| self.users_shown(c, cx)) else { return };
                let roles: Vec<_> = state.read(cx).visible_roles(cx).iter().map(|r| r.reference()).collect();
                let Some(next) = step(&roles, state.read(cx).selected.as_ref(), offset) else { return };
                self.show_role(next, window, cx);
                self.roles_focus.focus(window, cx);
            }
        }
        self.reveal.set(Some(list));
        cx.notify();
    }

    /// Scrolls `list` to child `ix` when the arrow keys just moved its selection there.
    fn reveal_child(&self, list: List, ix: Option<usize>) {
        if let (true, Some(ix)) = (self.reveal.get() == Some(list), ix) {
            self.reveal.set(None);
            let handle = match list {
                List::Connections => &self.connections_scroll,
                List::Tables => &self.tables_scroll,
                List::Roles => &self.roles_scroll,
            };
            handle.scroll_to_item(ix);
        }
    }

    /// Handles a File/View menu action meant for the active table tab.
    fn with_active_table(&mut self, window: &mut Window, cx: &mut Context<Self>, f: impl FnOnce(&mut TableTab, &mut Window, &mut Context<TableTab>)) {
        if let Some(TabView::Table(tab)) = self.tabs.get(self.active).map(|t| &t.view) {
            tab.clone().update(cx, |tab, cx| f(tab, window, cx));
        }
    }

    fn show_users(&mut self, _: &ShowUsers, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_connection().is_some_and(|c| dbcore::access::features(c.kind).is_some()) {
            self.users_mode = true;
            cx.notify();
        }
    }

    fn toggle_schema(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(name) {
            self.collapsed.insert(name.to_string());
        }
        cx.notify();
    }

    // MARK: users

    /// The selected connection's users, when the middle column lists them.
    fn users_shown(&self, connection: &ConnectionConfig, cx: &App) -> Option<Entity<UsersState>> {
        let _ = cx;
        if !self.users_mode {
            return None;
        }
        self.users.get(&connection.id).map(|(state, _)| state.clone())
    }

    /// Makes the selected connection's users (once it's connected), or points them at the database
    /// shown (Postgres privileges are per database).
    fn sync_users(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.selected_connection().cloned() else { return };
        let Some(features) = dbcore::access::features(config.kind) else { return };
        let Some((key, connection)) = self.open_connection() else { return };
        if let Some((state, _)) = self.users.get(&config.id) {
            if state.read(cx).key != key {
                state.update(cx, |s, cx| s.retarget(connection, key, cx));
            }
            return;
        }
        let state = cx.new(|cx| UsersState::new(connection, key, features, window, cx));
        let tab = cx.new(|cx| UsersTab::new(state.clone(), cx));
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        // A role made in the editor opens in the tab.
        cx.subscribe_in(&state, window, |this, _, event: &RoleCreated, window, cx| this.show_role(event.0.clone(), window, cx)).detach();
        self.users.insert(config.id, (state, tab));
    }

    /// Selects a role and shows it in the connection's users tab.
    fn show_role(&mut self, role: dbcore::access::RoleRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else { return };
        let Some((state, tab)) = self.users.get(&id).cloned() else { return };
        state.update(cx, |s, cx| s.select(role, cx));
        let existing = self.tabs.iter().position(|t| matches!(&t.view, TabView::Users(u) if *u == tab));
        let ix = match existing {
            Some(ix) => ix,
            None => {
                let key = state.read(cx).key.clone();
                let view = TabView::Users(tab);
                let events = self.tab_events(&key, &view, window, cx);
                let ix = if self.tabs.is_empty() { 0 } else { (self.active + 1).min(self.tabs.len()) };
                self.tabs.insert(ix, OpenTab { key, view, _events: events });
                ix
            }
        };
        self.activate(ix, window, cx);
    }

    fn render_users(&self, state: Entity<UsersState>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let s = state.read(cx);
        match &s.roles {
            users::Load::Idle | users::Load::Loading => return centered().child(Spinner::new()).into_any_element(),
            users::Load::Failed(message) => {
                let retry = state.clone();
                return centered()
                    .p_4()
                    .gap_2()
                    .child(Icon::new(IconName::TriangleAlert).text_color(theme.warning))
                    .child(div().font_semibold().child(format!("Couldn’t List {}s", users::noun(s.kind()))))
                    .child(div().text_sm().text_color(theme.muted_foreground).text_center().child(message.clone()))
                    .child(Button::new("retry-users").small().label("Try Again").on_click(move |_, _, cx| {
                        retry.update(cx, |s, cx| s.load_roles(None, cx));
                    }))
                    .into_any_element();
            }
            users::Load::Loaded(_) => {}
        }
        let kind = s.kind();
        let hosts = s.features.hosts;
        let roles = s.visible_roles(cx);
        let tab_active = self.tabs.get(self.active).is_some_and(|t| matches!(&t.view, TabView::Users(u) if self.users.values().any(|(st, ut)| st == &state && ut == u)));
        let selected = s.selected.clone().filter(|_| tab_active);
        let show_system = s.show_system;
        let empty = if s.search.read(cx).value().trim().is_empty() { format!("No {}s", users::noun(kind)) } else { "No Matches".to_string() };
        let mut list = v_flex()
            .id("roles")
            .size_full()
            .p_2()
            .gap_px()
            .key_context("RoleList")
            .track_focus(&self.roles_focus)
            .on_action(cx.listener(|this, _: &SelectPrevious, window, cx| this.move_selection(List::Roles, -1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| this.move_selection(List::Roles, 1, window, cx)))
            .overflow_y_scroll()
            .track_scroll(&self.roles_scroll);
        let selected_ix = roles.iter().position(|r| selected.as_ref() == Some(&r.reference()));
        self.reveal_child(List::Roles, selected_ix);
        for role in &roles {
            let reference = role.reference();
            let is_selected = selected.as_ref() == Some(&reference);
            list = list.child(
                row(SharedString::from(format!("role-{}", reference.title())), is_selected, cx)
                    .when(role.is_system, |r| r.opacity(0.6))
                    .child(users::role_icon(role).small().text_color(if is_selected { theme.primary } else { theme.muted_foreground }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(role.name.clone()))
                            .children(role.host.clone().filter(|_| hosts).map(|h| div().text_xs().text_color(theme.muted_foreground).child(h))),
                    )
                    .when(role.is_superuser, |r| r.child(Icon::new(AssetIcon::Zap).xsmall().text_color(theme.warning)))
                    .on_click({
                        let reference = reference.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.show_role(reference.clone(), window, cx);
                            this.roles_focus.focus(window, cx);
                        })
                    })
                    .context_menu({
                        let (this, state, role) = (cx.entity().downgrade(), state.clone(), role.clone());
                        move |menu, _, _| {
                            let reference = role.reference();
                            let (edit_state, edit_role) = (state.clone(), role.clone());
                            let (grant_this, grant_role) = (this.clone(), reference.clone());
                            let grant_state = state.clone();
                            let copy_role = role.clone();
                            let new_state = state.clone();
                            let (drop_state, drop_role) = (state.clone(), role.clone());
                            menu.item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                                users::open_role_editor(edit_state.clone(), Some(edit_role.clone()), window, cx)
                            }))
                            .item(PopupMenuItem::new("Grant Privileges…").on_click(move |_, window, cx| {
                                grant_this.update(cx, |w, cx| w.show_role(grant_role.clone(), window, cx)).ok();
                                users::open_privilege_editor(grant_state.clone(), grant_role.clone(), None, window, cx)
                            }))
                            .item(PopupMenuItem::new("Copy Name").on_click(move |_, _, cx| users::copy_name(&copy_role, cx)))
                            .separator()
                            .item(PopupMenuItem::new(format!("New {}…", users::noun(kind))).on_click(move |_, window, cx| {
                                users::open_role_editor(new_state.clone(), None, window, cx)
                            }))
                            .item(PopupMenuItem::new("Drop…").disabled(role.is_system).on_click(move |_, window, cx| {
                                users::confirm_drop(drop_state.clone(), drop_role.clone(), window, cx)
                            }))
                        }
                    }),
            );
        }
        if roles.is_empty() {
            list = list.child(div().mt_4().px_2().text_sm().text_color(theme.muted_foreground).child(empty));
        }
        let toggle_state = state.clone();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .pt_2()
                    .gap_1()
                    .child(div().flex_1().child(gpui_kit::component::input::Input::new(&s.search).small().cleanable(true)))
                    .child(
                        Button::new("show-system")
                            .ghost()
                            .small()
                            .icon(Icon::new(AssetIcon::KeyRound))
                            .selected(show_system)
                            .tooltip(if show_system { "Hide Built-in Roles" } else { "Show Built-in Roles" })
                            .on_click(move |_, _, cx| {
                                toggle_state.update(cx, |s, cx| {
                                    s.show_system = !s.show_system;
                                    cx.notify();
                                })
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().child(list.vertical_scrollbar(&self.roles_scroll)))
            .into_any_element()
    }

    // MARK: columns

    fn render_connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let theme = &theme;
        let selected_row = self.selected_row();
        let mut rows: Vec<AnyElement> = Vec::new();
        // The selected row's index among `rows`, to scroll to it after an arrow key.
        let mut selected_ix = None;
        for (group, connections) in self.grouped_connections() {
            let collapsed = self.collapsed_groups.contains(&group);
            let title = if group.is_empty() { "Connections".to_string() } else { group.clone() };
            rows.push(group_header(title, collapsed, theme).on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&group) {
                    this.collapsed_groups.insert(group.clone());
                }
                cx.notify();
            })).into_any_element());
            if collapsed {
                continue;
            }
            for c in connections {
                let row_id: SidebarRow = (c.id.clone(), None);
                let selected = selected_row.as_ref() == Some(&row_id);
                if selected {
                    selected_ix = Some(rows.len());
                }
                rows.push(self.connection_row(c, selected, cx).into_any_element());
                if !self.shows_databases(c) {
                    continue;
                }
                for database in self.sidebar_databases(c).into_iter().flatten() {
                    let row_id: SidebarRow = (c.id.clone(), Some(database.clone()));
                    let selected = selected_row.as_ref() == Some(&row_id);
                    if selected {
                        selected_ix = Some(rows.len());
                    }
                    rows.push(self.database_row(c, &database, selected, cx).into_any_element());
                }
            }
        }
        self.reveal_child(List::Connections, selected_ix);
        let mut list = v_flex()
            .id("connections")
            .size_full()
            .p_2()
            .gap_px()
            .key_context("ConnectionList")
            .track_focus(&self.connections_focus)
            .on_action(cx.listener(|this, _: &SelectPrevious, window, cx| this.move_selection(List::Connections, -1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| this.move_selection(List::Connections, 1, window, cx)))
            .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                if let (Some(id), false) = (this.selected.clone(), this.showing_samples) {
                    this.confirm_delete(id, window, cx);
                }
            }))
            .overflow_y_scroll()
            .track_scroll(&self.connections_scroll)
            .children(rows);
        let notices = self
            .showing_samples
            .then(|| "No saved connections yet; showing the samples.".to_string())
            .into_iter()
            .chain(self.notice.clone());
        for notice in notices {
            list = list.child(div().mt_4().px_2().text_xs().text_color(theme.muted_foreground).child(notice));
        }
        if self.showing_samples {
            list = list.child(
                h_flex().mt_1().px_1().child(
                    Button::new("import-samples")
                        .link()
                        .small()
                        .label("Import from DBeaver…")
                        .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
                ),
            );
        }
        let list = list.vertical_scrollbar(&self.connections_scroll);
        v_flex().size_full().bg(theme.sidebar).child(div().flex_1().min_h_0().child(list)).child(
            h_flex().p_2().border_t_1().border_color(theme.sidebar_border).child(
                Button::new("new-connection")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .label("New Connection")
                    .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
            )
            .child(div().flex_1())
            .child(crate::update::footer_button(cx))
            .child(
                Button::new("import-connections")
                    .ghost()
                    .small()
                    .icon(Icon::new(AssetIcon::Import))
                    .tooltip("Import from DBeaver…")
                    .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
            ),
        )
    }

    /// A connection in the sidebar: engine logo, name, then two fixed slots so status lights and
    /// warnings line up in one column: the status, and the chevron that lists its databases.
    fn connection_row(&self, c: &ConnectionConfig, selected: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let connected = self.is_open(&c.id);
        let lit = connected && !self.lost.contains(&c.id);
        let failed = !connected && self.failed.contains(&c.id);
        let expandable = self.sidebar_databases(c).is_some();
        let expanded = self.expanded.contains(&c.id);
        let id = c.id.clone();
        let status = div()
            .flex_shrink_0()
            .w(px(18.))
            .flex()
            .justify_center()
            .when(lit, |slot| slot.child(connected_indicator(SharedString::from(format!("lit-{}", c.id)), theme.green)))
            .when(failed, |slot| slot.child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.muted_foreground)));
        let chevron = div().flex_shrink_0().w(px(16.)).flex().justify_center().when(expandable, |slot| {
            let id = id.clone();
            slot.child(
                Button::new(SharedString::from(format!("expand-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).text_color(theme.muted_foreground))
                    .tooltip(if expanded { "Hide Databases" } else { "Show Databases" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if !this.expanded.remove(&id) {
                            this.expanded.insert(id.clone());
                        }
                        cx.notify();
                    })),
            )
        });
        row(SharedString::from(format!("conn-{}", c.id)), selected, cx)
            .child(crate::assets::kind_icon(c.kind).small().text_color(if selected { theme.primary } else { theme.muted_foreground }))
            .child(div().flex_1().truncate().child(c.name.clone()))
            .child(status)
            .child(chevron)
            .tooltip({
                let summary = SharedString::from(if failed { format!("{} · Couldn’t connect", c.summary()) } else { c.summary() });
                move |window, cx| Tooltip::new(summary.clone()).build(window, cx)
            })
            .on_click({
                let id = id.clone();
                cx.listener(move |this, _, window, cx| {
                    this.select_connection(id.clone(), cx);
                    this.connections_focus.focus(window, cx);
                    cx.notify();
                })
            })
            .context_menu({
                let this = cx.entity().downgrade();
                let config = c.clone();
                move |menu, _, _| {
                    let (edit, delete) = (this.clone(), this.clone());
                    let (connect, duplicate, copy_url, refresh) = (this.clone(), this.clone(), this.clone(), this.clone());
                    let (config, id) = (config.clone(), config.id.clone());
                    let import = this.clone();
                    let (connect_id, refresh_id, script_id) = (id.clone(), id.clone(), id.clone());
                    let script = this.clone();
                    let (duplicate_config, url_config) = (config.clone(), config.clone());
                    let menu = if connected {
                        menu.item(PopupMenuItem::new("Disconnect").on_click(move |_, _, cx| {
                            connect.update(cx, |w, cx| w.disconnect(&connect_id, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Refresh").on_click(move |_, _, cx| {
                            let id = refresh_id.clone();
                            refresh
                                .update(cx, |w, cx| {
                                    w.select_connection(id, cx);
                                    w.load_schemas_with(true, false, cx);
                                })
                                .ok();
                        }))
                    } else {
                        menu.item(PopupMenuItem::new("Connect").on_click(move |_, _, cx| {
                            let id = connect_id.clone();
                            connect.update(cx, |w, cx| w.connect(id, cx)).ok();
                        }))
                    };
                    let menu = menu.item(PopupMenuItem::new("New SQL Script").on_click(move |_, window, cx| {
                        let id = script_id.clone();
                        script.update(cx, |w, cx| w.new_script_on(id, None, window, cx)).ok();
                    }));
                    let new_database = this.clone();
                    let new_database_config = config.clone();
                    let (dump_this, dump_config) = (this.clone(), config.clone());
                    let menu = menu.when(can_create_databases(&config), |menu| {
                        menu.item(PopupMenuItem::new("New Database…").on_click(move |_, window, cx| {
                            let config = new_database_config.clone();
                            new_database.update(cx, |w, cx| w.open_new_database(config, window, cx)).ok();
                        }))
                    });
                    menu.separator()
                        .item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                            let config = config.clone();
                            edit.update(cx, |w, cx| w.open_editor(Some(config), window, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Duplicate").on_click(move |_, _, cx| {
                            let config = duplicate_config.clone();
                            duplicate.update(cx, |w, cx| w.duplicate(config, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Copy URL").on_click(move |_, _, cx| {
                            let config = url_config.clone();
                            copy_url.update(cx, |w, cx| w.copy_url(config, cx)).ok();
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                            let id = id.clone();
                            delete.update(cx, |w, cx| w.confirm_delete(id, window, cx)).ok();
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Dump Database…").on_click({
                            let (this, config) = (dump_this.clone(), dump_config.clone());
                            move |_, window, cx| {
                                let config = config.clone();
                                this.update(cx, |w, cx| w.open_dump(config, DumpPreset::Database, window, cx)).ok();
                            }
                        }))
                        .item(PopupMenuItem::new("Restore from File…").on_click({
                            let (this, config) = (dump_this.clone(), dump_config.clone());
                            move |_, window, cx| {
                                let config = config.clone();
                                this.update(cx, |w, cx| w.open_restore(config, window, cx)).ok();
                            }
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Import from DBeaver…").on_click(move |_, window, cx| {
                            import.update(cx, |w, cx| w.open_import(window, cx)).ok();
                        }))
                }
            })
    }

    /// One of an expanded connection's databases, under it in the sidebar.
    fn database_row(&self, c: &ConnectionConfig, database: &str, selected: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let tooltip = SharedString::from(if database == c.default_database() { format!("{database} (default)") } else { database.to_string() });
        let row_id: SidebarRow = (c.id.clone(), Some(database.to_string()));
        let script_row = row_id.clone();
        row(SharedString::from(format!("db-{}-{database}", c.id)), selected, cx)
            .pl(px(28.))
            .child(Icon::new(AssetIcon::Database).small().text_color(if selected { theme.primary } else { theme.muted_foreground }))
            .child(div().flex_1().truncate().child(database.to_string()))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_row(row_id.clone(), cx);
                this.connections_focus.focus(window, cx);
            }))
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let (this, (id, database)) = (this.clone(), script_row.clone());
                    menu.item(PopupMenuItem::new("New SQL Script").on_click(move |_, window, cx| {
                        let (id, database) = (id.clone(), database.clone());
                        this.update(cx, |w, cx| w.new_script_on(id, database, window, cx)).ok();
                    }))
                }
            })
    }

    fn render_tables(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Cloned: the users list and the mode switch need `cx` while building.
        let theme = cx.theme().clone();
        let theme = &theme;
        let Some(connection) = self.selected_connection() else {
            return v_flex().size_full().bg(theme.background);
        };
        let users = self.users_shown(connection, cx);
        let subtitle = match &self.schemas {
            _ if users.is_some() => {
                let state = users.as_ref().unwrap().read(cx);
                match state.roles.value() {
                    Some(roles) => {
                        let shown = roles.iter().filter(|r| state.show_system || !r.is_system).count();
                        let n = users::count_label(state.kind(), shown);
                        if state.features.grants_per_database { format!("{n} · privileges in {}", state.database()) } else { n }
                    }
                    None => "Loading…".into(),
                }
            }
            Load::Loaded(_) if self.tables_only => "Filter by: Tables only".into(),
            Load::Loaded(schemas) => {
                let tables: usize = schemas.iter().map(|s| s.tables.len()).sum();
                format!("{} · {tables} tables", plural(schemas.len(), "schema"))
            }
            Load::Loading => "Loading…".into(),
            _ => connection.summary(),
        };
        let title = match self.service.databases(&connection.id).filter(|_| dbservice::browses_databases(connection)) {
            Some(databases) => {
                let this = cx.entity().downgrade();
                let current = self.database.clone();
                let config = connection.clone();
                // A MySQL connection with no database of its own browses them all: no database to show.
                let label = if self.database.is_empty() { connection.name.clone() } else { self.database.clone() };
                Button::new("database-menu")
                    .ghost()
                    .compact()
                    .label(label)
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _, _| {
                        for database in &databases {
                            let (this, database) = (this.clone(), database.clone());
                            menu = menu.item(PopupMenuItem::new(database.clone()).checked(database == current).on_click(
                                move |_, _, cx| {
                                    let database = database.clone();
                                    this.update(cx, |this, cx| this.select_database(database, cx)).ok();
                                },
                            ));
                        }
                        let (this, config) = (this.clone(), config.clone());
                        menu.when(can_create_databases(&config), |menu| {
                            menu.separator().item(PopupMenuItem::new("New Database…").on_click(move |_, window, cx| {
                                let config = config.clone();
                                this.update(cx, |w, cx| w.open_new_database(config, window, cx)).ok();
                            }))
                        })
                    })
                    .into_any_element()
            }
            None => div().font_semibold().truncate().child(connection.name.clone()).into_any_element(),
        };
        let connected = self.open_connection().is_some();
        let active_table = self.active_table(cx);
        let header = v_flex()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex().gap_1().child(h_flex().flex_1().min_w_0().child(title)).when(users.is_some(), |h| {
                    let state = users.clone().unwrap();
                    let kind = state.read(cx).kind();
                    h.child(
                        Button::new("new-role")
                            .ghost()
                            .small()
                            .icon(Icon::new(AssetIcon::UserPlus))
                            .tooltip(format!("New {}…", users::noun(kind)))
                            .on_click(move |_, window, cx| users::open_role_editor(state.clone(), None, window, cx)),
                    )
                }).when(users.is_none(), |h| h.child(
                    Button::new("tables-only")
                        .ghost()
                        .small()
                        .icon(Icon::new(AssetIcon::ListFilter))
                        .selected(self.tables_only)
                        .tooltip(if self.tables_only { "Show Views Too" } else { "Show Tables Only" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tables_only = !this.tables_only;
                            cx.notify();
                        })),
                )).child(
                    Button::new("new-script")
                        .ghost()
                        .small()
                        .icon(IconName::SquareTerminal)
                        .disabled(!connected)
                        .tooltip(format!("New SQL Script ({})", crate::keys::shortcut("secondary-t")))
                        .on_click(cx.listener(|this, _, window, cx| this.new_script(&NewScript, window, cx))),
                ).child({
                    let this = cx.entity().downgrade();
                    let schema_names: Vec<String> = match &self.schemas {
                        Load::Loaded(schemas) => schemas.iter().map(|s| s.name.clone()).collect(),
                        _ => Vec::new(),
                    };
                    let backup_config = connection.clone();
                    Button::new("tables-menu").ghost().small().icon(IconName::Ellipsis).dropdown_menu(move |menu, _, _| {
                        let (refresh, expand, collapse) = (this.clone(), this.clone(), this.clone());
                        let names = schema_names.clone();
                        let (dump, restore, config) = (this.clone(), this.clone(), backup_config.clone());
                        let restore_config = config.clone();
                        let menu = menu
                            .item(PopupMenuItem::new("Dump Database…").on_click(move |_, window, cx| {
                                let config = config.clone();
                                dump.update(cx, |w, cx| w.open_dump(config, DumpPreset::Database, window, cx)).ok();
                            }))
                            .item(PopupMenuItem::new("Restore from File…").on_click(move |_, window, cx| {
                                let config = restore_config.clone();
                                restore.update(cx, |w, cx| w.open_restore(config, window, cx)).ok();
                            }))
                            .separator();
                        menu.item(PopupMenuItem::new("Refresh").on_click(move |_, _, cx| {
                            refresh.update(cx, |w, cx| w.load_schemas_with(true, false, cx)).ok();
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Expand All").on_click(move |_, _, cx| {
                            expand
                                .update(cx, |w, cx| {
                                    w.collapsed.clear();
                                    cx.notify();
                                })
                                .ok();
                        }))
                        .item(PopupMenuItem::new("Collapse All").on_click(move |_, _, cx| {
                            let names = names.clone();
                            collapse
                                .update(cx, |w, cx| {
                                    w.collapsed = names.into_iter().collect();
                                    cx.notify();
                                })
                                .ok();
                        }))
                    })
                }),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(subtitle))
            .when(dbcore::access::features(connection.kind).is_some(), |header| {
                let users_mode = self.users_mode;
                let switch = |id: &'static str, label: &'static str, users: bool, cx: &mut Context<Self>| {
                    Button::new(id).ghost().xsmall().label(label).selected(users_mode == users).on_click(cx.listener(move |this, _, _, cx| {
                        this.users_mode = users;
                        cx.notify();
                    }))
                };
                header.child(
                    h_flex()
                        .mt_1()
                        .gap_1()
                        .child(switch("mode-tables", "Tables", false, cx))
                        .child(switch("mode-users", if connection.kind == DatabaseKind::Mysql { "Users" } else { "Roles" }, true, cx)),
                )
            });

        let body = match &self.schemas {
            _ if users.is_some() => self.render_users(users.clone().unwrap(), cx),
            Load::Idle | Load::Loading => centered().child(Spinner::new()).into_any_element(),
            Load::Failed(message) => centered()
                .p_4()
                .gap_2()
                .child(Icon::new(IconName::TriangleAlert).text_color(theme.warning))
                .child(div().font_semibold().child("Couldn’t Connect"))
                .child(div().text_sm().text_color(theme.muted_foreground).text_center().child(message.clone()))
                .into_any_element(),
            Load::Loaded(schemas) => {
                let mut list = v_flex()
                    .id("tables")
                    .size_full()
                    .p_2()
                    .gap_px()
                    .key_context("TableList")
                    .track_focus(&self.tables_focus)
                    .on_action(cx.listener(|this, _: &SelectPrevious, window, cx| this.move_selection(List::Tables, -1, window, cx)))
                    .on_action(cx.listener(|this, _: &SelectNext, window, cx| this.move_selection(List::Tables, 1, window, cx)))
                    .overflow_y_scroll()
                    .track_scroll(&self.tables_scroll);
                let mut children = 0;
                let mut selected_ix = None;
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
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_schema(&name, cx)))
                            .context_menu({
                                let (this, config, schema) = (cx.entity().downgrade(), connection.clone(), schema.name.clone());
                                move |menu, _, _| {
                                    let (this, config, schema) = (this.clone(), config.clone(), schema.clone());
                                    menu.item(PopupMenuItem::new(format!("Dump “{schema}”…")).on_click(move |_, window, cx| {
                                        let (config, preset) = (config.clone(), DumpPreset::Schema(schema.clone()));
                                        this.update(cx, |w, cx| w.open_dump(config, preset, window, cx)).ok();
                                    }))
                                }
                            }),
                    );
                    children += 1;
                    if collapsed {
                        continue;
                    }
                    for table in schema.tables.iter().filter(|t| !self.tables_only || t.kind == TableKind::Table) {
                        let selected = active_table.as_ref() == Some(table);
                        if selected {
                            selected_ix = Some(children);
                        }
                        children += 1;
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
                                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                    this.open_table(open.clone(), event.click_count() >= 2, window, cx);
                                    // A single click keeps the arrow keys on the list; a double click
                                    // (keep the tab open) goes to its rows.
                                    if event.click_count() < 2 {
                                        this.tables_focus.focus(window, cx);
                                    }
                                }))
                                .context_menu({
                                    let (this, config, table) = (cx.entity().downgrade(), connection.clone(), table.clone());
                                    move |menu, _, _| {
                                        let (this, config, table) = (this.clone(), config.clone(), table.clone());
                                        menu.item(PopupMenuItem::new(format!("Dump “{}”…", table.name)).on_click(move |_, window, cx| {
                                            let (config, preset) = (config.clone(), DumpPreset::Table(table.clone()));
                                            this.update(cx, |w, cx| w.open_dump(config, preset, window, cx)).ok();
                                        }))
                                    }
                                }),
                        );
                    }
                }
                self.reveal_child(List::Tables, selected_ix);
                list.vertical_scrollbar(&self.tables_scroll).into_any_element()
            }
        };
        v_flex().size_full().bg(theme.background).child(header).child(div().flex_1().min_h_0().child(body))
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(active) = self.tabs.get(self.active) else {
            return centered()
                .bg(theme.background)
                .text_2xl()
                .text_color(theme.muted_foreground.opacity(0.6))
                .child("No Table Selected")
                .into_any_element();
        };
        let content = match &active.view {
            TabView::Table(tab) => tab.clone().into_any_element(),
            TabView::Script(tab) => tab.clone().into_any_element(),
            TabView::Users(tab) => tab.clone().into_any_element(),
            TabView::Result(tab) => tab.clone().into_any_element(),
        };
        // Like the macOS app: no tab bar while there's only one tab.
        let bar = (self.tabs.len() > 1).then(|| {
            let mut bar = TabBar::new("tabs").selected_index(self.active).on_click(cx.listener(
                |this, ix: &usize, window, cx| this.activate(*ix, window, cx),
            ));
            for (ix, tab) in self.tabs.iter().enumerate() {
                let close = Button::new(("close-tab", ix))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab_at(ix, window, cx)
                    }));
                // The tab pads its label on both sides but not the suffix, so the
                // button would sit far from the title and flush against the right
                // edge. Pull it into the label's padding and pad its right side.
                let close = div().ml(px(-8.)).pr_2().child(close);
                let title = SharedString::from(tab.title(cx));
                let drop_color = theme.drag_border;
                bar = bar.child(
                    TabItem::new()
                        .label(title.clone())
                        .suffix(close)
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.open_tab_menu(ix, event.position, window, cx)
                            }),
                        )
                        .on_drag(DraggedTab { ix, title }, |tab, _, _, cx| cx.new(|_| tab.clone()))
                        .drag_over::<DraggedTab>(move |style, _, _, _| style.border_l_2().border_color(drop_color))
                        .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| this.move_tab(dragged.ix, ix, cx))),
                );
            }
            bar
        });
        v_flex()
            .size_full()
            .bg(theme.background)
            .children(bar)
            .child(div().flex_1().min_h_0().child(content))
            .children(self.tab_menu.clone().map(|(menu, position)| {
                deferred(anchored().position(position).snap_to_window_with_margin(px(8.)).child(menu)).with_priority(1)
            }))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.users_mode {
            self.sync_users(window, cx);
        }
        self.open_pending_script(window, cx);
        v_flex()
            .size_full()
            .relative()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::new_script))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab1, window, cx| this.select_tab(0, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab2, window, cx| this.select_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab3, window, cx| this.select_tab(2, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab4, window, cx| this.select_tab(3, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab5, window, cx| this.select_tab(4, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab6, window, cx| this.select_tab(5, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab7, window, cx| this.select_tab(6, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab8, window, cx| this.select_tab(7, window, cx)))
            .on_action(cx.listener(|this, _: &LastTab, window, cx| {
                let last = this.tabs.len().saturating_sub(1);
                this.select_tab(last, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| this.open_editor(None, window, cx)))
            .on_action(cx.listener(|this, _: &EditConnection, window, cx| {
                if let Some(config) = this.selected_connection().cloned() {
                    this.open_editor(Some(config), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Disconnect, _, cx| {
                if let Some(id) = this.selected.clone() {
                    this.disconnect(&id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DisconnectAll, _, cx| this.disconnect_all(cx)))
            .on_action(cx.listener(|this, _: &ImportConnections, window, cx| this.open_import(window, cx)))
            .on_action(cx.listener(Self::show_users))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(|this, _: &ShowData, window, cx| this.with_active_table(window, cx, |t, w, cx| t.show_structure(false, w, cx))))
            .on_action(cx.listener(|this, _: &ShowStructure, window, cx| this.with_active_table(window, cx, |t, w, cx| t.show_structure(true, w, cx))))
            .on_action(cx.listener(|this, _: &AddRow, window, cx| this.with_active_table(window, cx, |t, w, cx| t.add_row_from_menu(w, cx))))
            .children(self.menu_bar.clone().map(|bar| {
                div().flex_shrink_0().h(px(30.)).px_1().border_b_1().border_color(cx.theme().border).bg(cx.theme().title_bar).child(bar)
            }))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace")
                        .child(
                            resizable_panel()
                                .visible(self.sidebar_visible)
                                .size(px(240.))
                                .size_range(px(180.)..px(400.))
                                .child(self.render_connections(cx)),
                        )
                        .child(resizable_panel().size(px(300.)).size_range(px(200.)..px(520.)).child(self.render_tables(cx)))
                        .child(resizable_panel().child(self.render_tabs(cx))),
                ),
            )
            // Running and finished dumps/restores, over the bottom-right corner.
            .child(div().absolute().bottom_4().right_4().child(self.backups.clone()))
    }
}

/// Whether "New Database…" applies: the connection browses its server's databases and the core can
/// write a `create database` for it (not D1's, which are made through Cloudflare's API or wrangler).
fn can_create_databases(config: &ConnectionConfig) -> bool {
    config.supports_multiple_databases() && Dialect(config.kind).create_database("db").is_ok()
}

/// A sidebar group's header (Local, Production…). Clicking it folds the group, like Mail's; the
/// chevron shows on hover, and stays while the group is folded.
fn group_header(title: String, collapsed: bool, theme: &gpui_kit::component::Theme) -> Stateful<Div> {
    let group = SharedString::from(format!("group-{title}"));
    h_flex()
        .id(group.clone())
        .group(group.clone())
        .mt_3()
        .mb_1()
        .px_2()
        .text_xs()
        .font_semibold()
        .text_color(theme.muted_foreground)
        .child(div().flex_1().truncate().child(title))
        .child(
            div()
                .when(!collapsed, |d| d.invisible().group_hover(group, |s| s.visible()))
                .child(Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).xsmall()),
        )
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

/// The status light of an open connection, like the macOS app's: a lit dot with a soft halo and a
/// glow, that sends out one ripple when it appears.
fn connected_indicator(id: SharedString, green: Hsla) -> impl IntoElement {
    const BOX: f32 = 14.;
    const DOT: f32 = 7.;
    let centered = |size: f32| div().absolute().left(px((BOX - size) / 2.)).top(px((BOX - size) / 2.)).size(px(size)).rounded_full();
    let lighter = Hsla { l: (green.l + 0.12).min(1.), ..green };
    let ripple = centered(DOT).border_1().border_color(green.opacity(0.6)).with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(1100)).with_easing(ease_out_quint()),
        move |ring, delta| {
            let size = DOT * (1. + 1.4 * delta);
            ring.left(px((BOX - size) / 2.)).top(px((BOX - size) / 2.)).size(px(size)).opacity(1. - delta)
        },
    );
    div()
        .relative()
        .flex_shrink_0()
        .size(px(BOX))
        .child(centered(13.).bg(green.opacity(0.18)))
        .child(ripple)
        .child(
            centered(DOT)
                .bg(linear_gradient(180., linear_color_stop(lighter, 0.), linear_color_stop(green, 1.)))
                .border_1()
                .border_color(gpui_kit::white().opacity(0.25))
                .shadow(vec![BoxShadow {
                    color: green.opacity(0.7),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(2.5),
                    spread_radius: px(0.),
                    inset: false,
                }]),
        )
}

fn centered() -> Div {
    v_flex().size_full().items_center().justify_center()
}
