//! The main window: connections | schemas and tables | tabs (tables and SQL scripts), like the macOS app.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use dbcore::secrets::{self, KeyringSecretStore};
use dbcore::state::StateStore;
use dbcore::{Connection, ConnectionConfig, ConnectionStore, Schema, TableInfo, TableKind};
use serde::{Deserialize, Serialize};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab as TabItem, TabBar};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::connection_editor::ConnectionEditor;
use crate::tabs::{History, ScriptTab, TableTab, count, plural};

enum Load<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(String),
}

/// One connection per (connection id, database): Postgres can't switch databases on a session.
type TargetKey = (String, String);

actions!(dbear, [NewScript, CloseTab]);

/// Key of the open tabs in the state store.
const SESSION_KEY: &str = "gpui.session";

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
    Table { connection: String, database: String, schema: String, table: String, view: bool },
    Script { connection: String, database: String, name: String, sql: String },
}

/// A connection for `config`, with its saved password read off the UI thread (it can block or prompt).
async fn new_connection(
    config: ConnectionConfig,
    secrets: Option<Arc<KeyringSecretStore>>,
    executor: BackgroundExecutor,
) -> Arc<Connection> {
    let config = executor
        .spawn(async move {
            match secrets {
                Some(store) => secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config),
                None => config,
            }
        })
        .await;
    Arc::new(Connection::new(config))
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-t", NewScript, Some("Workspace")),
        KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
    ]);
}

enum TabView {
    Table(Entity<TableTab>),
    Script(Entity<ScriptTab>),
}

struct OpenTab {
    /// The connection and database it uses.
    key: TargetKey,
    view: TabView,
}

impl OpenTab {
    fn title(&self, cx: &App) -> String {
        match &self.view {
            TabView::Table(tab) => tab.read(cx).title(),
            TabView::Script(tab) => tab.read(cx).name.clone(),
        }
    }

    fn table(&self, cx: &App) -> Option<TableInfo> {
        match &self.view {
            TabView::Table(tab) => Some(tab.read(cx).table.clone()),
            TabView::Script(_) => None,
        }
    }
}

pub struct Workspace {
    store: Option<ConnectionStore>,
    /// Open tabs and query history (`state.db`, beside the connection store).
    state: Option<Rc<RefCell<StateStore>>>,
    /// Reopening last session's tabs; nothing is saved until it's done.
    restoring: bool,
    restore_task: Option<Task<()>>,
    _quit: Option<Subscription>,
    connections: Vec<ConnectionConfig>,
    /// Why the saved connections or passwords aren't available, shown under the list.
    notice: Option<String>,
    /// No saved connections: the list shows the samples.
    showing_samples: bool,
    secrets: Option<Arc<KeyringSecretStore>>,
    open: HashMap<TargetKey, Arc<Connection>>,
    /// Schemas already listed per target, so switching back to a database is instant.
    schema_cache: HashMap<TargetKey, Vec<Schema>>,
    /// The databases on each connection's server, once listed (connections that show all databases).
    databases: HashMap<String, Vec<String>>,
    selected: Option<String>,
    /// The database shown for the selected connection.
    database: String,
    schemas: Load<Vec<Schema>>,
    collapsed: HashSet<String>,
    tabs: Vec<OpenTab>,
    active: usize,
    /// Scripts opened so far, for their names ("SQL 1", "SQL 2"…).
    scripts_made: usize,
    focus: FocusHandle,
    // Dropping a task cancels it, so switching selection abandons the previous load.
    schemas_task: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut notices = Vec::new();
        let store = open_store().map_err(|e| notices.push(format!("Couldn’t open saved connections: {e}"))).ok();
        let (connections, showing_samples) = Self::listed_connections(store.as_ref());
        let secrets = match KeyringSecretStore::new() {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                notices.push(format!("Saved passwords are unavailable ({e})."));
                None
            }
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let state = store.as_ref().and_then(|s| match StateStore::open_beside(s.path()) {
            Ok(state) => Some(Rc::new(RefCell::new(state))),
            Err(e) => {
                log::warn!("couldn’t open the state file (no history or restored tabs): {e}");
                None
            }
        });
        // Script text isn't saved as you type: the last of it is saved when the app quits.
        let quit = cx.on_app_quit(|this, cx| {
            this.save_session(cx);
            async {}
        });
        Self {
            store,
            state,
            restoring: false,
            restore_task: None,
            _quit: Some(quit),
            connections,
            showing_samples,
            notice: (!notices.is_empty()).then(|| notices.join("\n")),
            secrets,
            open: HashMap::new(),
            schema_cache: HashMap::new(),
            databases: HashMap::new(),
            selected: None,
            database: String::new(),
            schemas: Load::Idle,
            collapsed: HashSet::new(),
            tabs: Vec::new(),
            active: 0,
            scripts_made: 0,
            focus,
            schemas_task: None,
        }
    }

    /// Saved connections, or the samples when there are none yet.
    fn listed_connections(store: Option<&ConnectionStore>) -> (Vec<ConnectionConfig>, bool) {
        match store.map(|s| s.connections().to_vec()).filter(|c| !c.is_empty()) {
            Some(connections) => (connections, false),
            None => (dbcore::mock::connections(), true),
        }
    }

    /// Drops everything opened or cached for connection `id` (it was edited or deleted), its tabs too.
    fn forget(&mut self, id: &str) {
        self.tabs.retain(|tab| tab.key.0 != id);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.open.retain(|(open, _), _| open != id);
        self.schema_cache.retain(|(cached, _), _| cached != id);
        self.databases.remove(id);
    }

    // MARK: editing connections

    fn open_editor(&mut self, original: Option<ConnectionConfig>, window: &mut Window, cx: &mut Context<Self>) {
        let title = if original.is_some() { "Edit Connection" } else { "New Connection" };
        let secrets = self.secrets.clone();
        let editor = cx.new(|cx| ConnectionEditor::new(original, secrets, window, cx));
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
                .child(div().flex_1())
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
            let _ = cx;
            dialog.title(title).w(px(560.)).child(editor).footer(footer)
        });
    }

    /// Saves the editor's connection (and password). Returns whether it worked; errors show in the form.
    fn save_connection(&mut self, editor: &Entity<ConnectionEditor>, cx: &mut Context<Self>) -> bool {
        let (config, password) = match editor.read(cx).config(cx) {
            Ok(config) => config,
            Err(e) => {
                editor.update(cx, |e2, cx| e2.show_error(e, cx));
                return false;
            }
        };
        let is_new = editor.read(cx).is_new();
        let Some(store) = self.store.as_mut() else {
            editor.update(cx, |e, cx| e.show_error("The connection store isn’t available.".into(), cx));
            return false;
        };
        let saved = match store.upsert(config) {
            Ok(saved) => saved,
            Err(e) => {
                editor.update(cx, |ed, cx| ed.show_error(e.to_string(), cx));
                return false;
            }
        };
        if let Some(password) = password {
            match &self.secrets {
                Some(secrets) => {
                    if let Err(e) = secrets::save_password(secrets.as_ref(), &saved.id, Some(&password)) {
                        editor.update(cx, |ed, cx| ed.show_error(format!("Saved, but the password wasn’t: {e}"), cx));
                    }
                }
                None => log::warn!("no keyring: the password for {} isn’t saved", saved.name),
            }
        }
        let (connections, showing_samples) = Self::listed_connections(self.store.as_ref());
        self.connections = connections;
        self.showing_samples = showing_samples;
        self.forget(&saved.id);
        // Reopen it with the new settings (or open the new one).
        if is_new || self.selected.as_deref() == Some(saved.id.as_str()) {
            self.selected = None;
            self.select_connection(saved.id, cx);
        }
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
        let Some(store) = self.store.as_mut() else { return };
        if let Err(e) = store.remove(id) {
            log::error!("couldn’t delete the connection: {e}");
            return;
        }
        if let Some(secrets) = &self.secrets {
            let _ = secrets::save_password(secrets.as_ref(), id, None);
        }
        if let Some(state) = &self.state {
            let _ = state.borrow_mut().clear_history(id);
        }
        self.forget(id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
            self.schemas = Load::Idle;
            self.schemas_task = None;
        }
        let (connections, showing_samples) = Self::listed_connections(self.store.as_ref());
        self.connections = connections;
        self.showing_samples = showing_samples;
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
        let database = self.remembered_database(config).unwrap_or_else(|| config.default_database().to_string());
        self.selected = Some(id);
        self.database = database;
        self.collapsed.clear();
        self.load_schemas(cx);
        self.save_session(cx);
    }

    /// Whether the title offers the server's other databases.
    fn browses_databases(config: &ConnectionConfig) -> bool {
        config.show_all_databases && config.supports_multiple_databases()
    }

    /// The database last picked for `config`, kept in the connection store (shared with the macOS app).
    fn remembered_database(&self, config: &ConnectionConfig) -> Option<String> {
        if !Self::browses_databases(config) {
            return None;
        }
        self.store.as_ref()?.last_database(&config.id).filter(|db| db != config.default_database())
    }

    fn select_database(&mut self, database: String, cx: &mut Context<Self>) {
        let Some(config) = self.selected_connection().cloned() else { return };
        if database == self.database {
            return;
        }
        let remembered = (database != config.default_database()).then_some(database.as_str());
        if let Some(store) = self.store.as_mut() {
            if let Err(e) = store.set_last_database(&config.id, remembered) {
                log::warn!("couldn’t remember the database: {e}");
            }
        }
        self.database = database;
        self.collapsed.clear();
        self.load_schemas(cx);
        self.save_session(cx);
    }

    fn target(&self) -> Option<(ConnectionConfig, TargetKey)> {
        let config = self.selected_connection()?;
        let config =
            if self.database == config.default_database() { config.clone() } else { config.with_database(&self.database) };
        let key = (config.id.clone(), self.database.clone());
        Some((config, key))
    }

    fn load_schemas(&mut self, cx: &mut Context<Self>) {
        let Some((config, key)) = self.target() else { return };
        if let Some(schemas) = self.schema_cache.get(&key) {
            self.schemas = Load::Loaded(schemas.clone());
            self.schemas_task = None;
            cx.notify();
            return;
        }
        self.schemas = Load::Loading;
        let existing = self.open.get(&key).cloned();
        let secrets = self.secrets.clone();
        let list_databases = Self::browses_databases(&config) && !self.databases.contains_key(&config.id);
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
            // Not fatal when it fails: the connection still works with its own database.
            let databases = if list_databases && result.is_ok() { connection.list_databases().await.ok() } else { None };
            this.update(cx, |this, cx| {
                let current = this.target().map(|(_, k)| k);
                if current.as_ref() != Some(&key) {
                    return;
                }
                this.schemas = match result {
                    Ok(schemas) => {
                        this.open.insert(key.clone(), connection);
                        this.schema_cache.insert(key.clone(), schemas.clone());
                        Load::Loaded(schemas)
                    }
                    Err(e) => Load::Failed(e.to_string()),
                };
                if let Some(databases) = databases {
                    let (id, database) = key;
                    let gone = !databases.contains(&database);
                    this.databases.insert(id.clone(), databases);
                    // The remembered database was dropped or renamed: back to the connection's own.
                    let default = this.selected_connection().map(|c| c.default_database().to_string());
                    if let Some(default) = default.filter(|d| gone && *d != database) {
                        if let Some(store) = this.store.as_mut() {
                            let _ = store.set_last_database(&id, None);
                        }
                        this.database = default;
                        this.load_schemas(cx);
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
        let connection = self.open.get(&key)?.clone();
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
        let existing = self.tabs.iter().position(|tab| tab.key == key && tab.table(cx).as_ref() == Some(&table));
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
        let view = cx.new(|cx| {
            let mut tab = TableTab::new(connection, table, window, cx);
            tab.preview = !pin;
            tab
        });
        let tab = OpenTab { key, view: TabView::Table(view) };
        let preview = self.tabs.iter().position(|tab| match &tab.view {
            TabView::Table(t) => t.read(cx).preview,
            TabView::Script(_) => false,
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
        let target = self.target_label(&key);
        let history = self.history_for(&key);
        let view = cx.new(|cx| ScriptTab::new(connection, name, target, history, window, cx));
        self.tabs.push(OpenTab { key, view: TabView::Script(view) });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn history_for(&self, key: &TargetKey) -> Option<History> {
        Some(History { state: self.state.clone()?, connection_id: key.0.clone(), database: key.1.clone() })
    }

    /// "conn · db" (or just the connection's name) for a script's header.
    fn target_label(&self, key: &TargetKey) -> String {
        match self.connections.iter().find(|c| c.id == key.0) {
            Some(c) if Self::browses_databases(c) && c.name != key.1 => format!("{} · {}", c.name, key.1),
            Some(c) => c.name.clone(),
            None => key.1.clone(),
        }
    }

    // MARK: session

    fn save_session(&self, cx: &App) {
        let Some(state) = &self.state else { return };
        if self.restoring {
            return;
        }
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                let (connection, database) = tab.key.clone();
                match &tab.view {
                    TabView::Table(t) => {
                        let table = &t.read(cx).table;
                        SavedTab::Table {
                            connection,
                            database,
                            schema: table.schema.clone(),
                            table: table.name.clone(),
                            view: table.kind == TableKind::View,
                        }
                    }
                    TabView::Script(s) => {
                        let s = s.read(cx);
                        SavedTab::Script { connection, database, name: s.name.clone(), sql: s.text(cx) }
                    }
                }
            })
            .collect();
        let session = Session {
            selected: self.selected.clone().map(|id| (id, self.database.clone())),
            active: self.active,
            tabs,
        };
        match serde_json::to_string(&session) {
            Ok(json) => {
                if let Err(e) = state.borrow_mut().set(SESSION_KEY, Some(&json)) {
                    log::warn!("couldn’t save the open tabs: {e}");
                }
            }
            Err(e) => log::warn!("couldn’t save the open tabs: {e}"),
        }
    }

    /// Reopens last session's selection and tabs (tabs whose connection fails are skipped).
    pub fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(json) = self.state.as_ref().and_then(|s| s.borrow().get(SESSION_KEY)) else { return };
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
        let secrets = self.secrets.clone();
        self.restore_task = Some(cx.spawn_in(window, async move |this, cx| {
            for saved in session.tabs {
                let key = match &saved {
                    SavedTab::Table { connection, database, .. } | SavedTab::Script { connection, database, .. } => {
                        (connection.clone(), database.clone())
                    }
                };
                let Ok(Some((config, existing))) = this.update(cx, |w, _| {
                    let config = w.connections.iter().find(|c| c.id == key.0)?;
                    let config =
                        if key.1 == config.default_database() { config.clone() } else { config.with_database(&key.1) };
                    Some((config, w.open.get(&key).cloned()))
                }) else {
                    continue;
                };
                let connection = match existing {
                    Some(connection) => connection,
                    None => {
                        let connection = new_connection(config, secrets.clone(), cx.background_executor().clone()).await;
                        if let Err(e) = connection.connect().await {
                            log::warn!("not reopening a tab on {}: {e}", key.0);
                            continue;
                        }
                        {
                            let mine = connection.clone();
                            this.update(cx, |w, _| w.open.entry(key.clone()).or_insert(mine).clone()).unwrap_or(connection)
                        }
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
            SavedTab::Table { schema, table, view, .. } => {
                let mut table = TableInfo::new(schema, table);
                if view {
                    table.kind = TableKind::View;
                }
                TabView::Table(cx.new(|cx| {
                    let mut tab = TableTab::new(connection, table, window, cx);
                    tab.preview = false;
                    tab
                }))
            }
            SavedTab::Script { name, sql, .. } => {
                // Keep numbering new scripts after the restored ones.
                if let Some(n) = name.strip_prefix("SQL ").and_then(|n| n.parse::<usize>().ok()) {
                    self.scripts_made = self.scripts_made.max(n);
                }
                let (target, history) = (self.target_label(&key), self.history_for(&key));
                TabView::Script(cx.new(|cx| {
                    let mut tab = ScriptTab::new(connection, name, target, history, window, cx);
                    tab.set_text(&sql, window, cx);
                    tab
                }))
            }
        };
        self.tabs.push(OpenTab { key, view });
        cx.notify();
    }

    fn close_tab_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = match self.tabs.get(ix).map(|t| &t.view) {
            Some(TabView::Table(tab)) => tab.read(cx).has_unsaved_edits(cx),
            Some(TabView::Script(_)) => false,
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

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            window.remove_window();
        } else {
            self.close_tab_at(self.active, window, cx);
        }
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
                let connected = self.open.keys().any(|(id, _)| *id == c.id);
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
                        .on_click({
                            let id = id.clone();
                            cx.listener(move |this, _, _, cx| this.select_connection(id.clone(), cx))
                        })
                        .context_menu({
                            let this = cx.entity().downgrade();
                            let config = c.clone();
                            move |menu, _, _| {
                                let (edit, delete) = (this.clone(), this.clone());
                                let (config, id) = (config.clone(), config.id.clone());
                                menu.item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                                    let config = config.clone();
                                    edit.update(cx, |w, cx| w.open_editor(Some(config), window, cx)).ok();
                                }))
                                .item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                                    let id = id.clone();
                                    delete.update(cx, |w, cx| w.confirm_delete(id, window, cx)).ok();
                                }))
                            }
                        }),
                );
            }
        }
        let notices = self
            .showing_samples
            .then(|| "No saved connections yet; showing the samples.".to_string())
            .into_iter()
            .chain(self.notice.clone());
        for notice in notices {
            list = list.child(div().mt_4().px_2().text_xs().text_color(theme.muted_foreground).child(notice));
        }
        v_flex().size_full().bg(theme.sidebar).child(div().flex_1().min_h_0().child(list)).child(
            h_flex().p_2().border_t_1().border_color(theme.sidebar_border).child(
                Button::new("new-connection")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .label("New Connection")
                    .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
            ),
        )
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
        let title = match self.databases.get(&connection.id).filter(|_| Self::browses_databases(connection)) {
            Some(databases) => {
                let this = cx.entity().downgrade();
                let (databases, current) = (databases.clone(), self.database.clone());
                Button::new("database-menu")
                    .ghost()
                    .compact()
                    .label(self.database.clone())
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
                        menu
                    })
                    .into_any_element()
            }
            None => div().font_semibold().truncate().child(connection.name.clone()).into_any_element(),
        };
        let connected = self.open_connection().is_some();
        let active_table = self
            .tabs
            .get(self.active)
            .filter(|tab| self.target().is_some_and(|(_, key)| key == tab.key))
            .and_then(|tab| tab.table(cx));
        let header = v_flex()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex().gap_2().child(h_flex().flex_1().min_w_0().child(title)).child(
                    Button::new("new-script")
                        .ghost()
                        .small()
                        .icon(IconName::SquareTerminal)
                        .disabled(!connected)
                        .tooltip("New SQL Script (⌘T)")
                        .on_click(cx.listener(|this, _, window, cx| this.new_script(&NewScript, window, cx))),
                ),
            )
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
                        let selected = active_table.as_ref() == Some(table);
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
                                    this.open_table(open.clone(), event.click_count() >= 2, window, cx)
                                })),
                        );
                    }
                }
                list.into_any_element()
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
                bar = bar.child(TabItem::new().label(tab.title(cx)).suffix(close));
            }
            bar
        });
        v_flex()
            .size_full()
            .bg(theme.background)
            .children(bar)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::new_script))
            .on_action(cx.listener(Self::close_tab))
            .child(
                h_resizable("workspace")
                    .child(
                        resizable_panel().size(px(240.)).size_range(px(180.)..px(400.)).child(self.render_connections(cx)),
                    )
                    .child(resizable_panel().size(px(300.)).size_range(px(200.)..px(520.)).child(self.render_tables(cx)))
                    .child(resizable_panel().child(self.render_tabs(cx))),
            )
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
