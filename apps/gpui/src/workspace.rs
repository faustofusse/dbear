//! The main window: connections | schemas and tables | tabs (tables and SQL scripts), like the macOS app.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use dbcore::dialect::Dialect;
use dbcore::secrets::{self, KeyringSecretStore, SecretStore as _};
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
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::connection_editor::ConnectionEditor;
use crate::import_dialog::ImportDialog;
use crate::grid::{RelatedRows, copy};
use crate::tabs::{History, ScriptTab, TabEvent, TableTab, count, plural};

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
    _events: Subscription,
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

    fn filter(&self, cx: &App) -> Option<String> {
        match &self.view {
            TabView::Table(tab) => tab.read(cx).applied_filter(cx),
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
    _focus_lost: Subscription,
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
        // When the focused element goes away (e.g. the grid, on switching to Structure), nothing would
        // have focus and the window's shortcuts (new script, close tab) would stop working.
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| window.focus(&this.focus, cx));
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
            _focus_lost: focus_lost,
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

    /// Lists the saved connections again. When the first ones replace the samples, whatever was
    /// open on a sample (tabs, connections, the selection) goes with them.
    fn reload_connections(&mut self) {
        let (connections, showing_samples) = Self::listed_connections(self.store.as_ref());
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
        let open: Vec<Arc<Connection>> = self.open.iter().filter(|((c, _), _)| c == id).map(|(_, c)| c.clone()).collect();
        self.forget(id);
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

    /// Adds a copy named "… copy", with the same password, and selects it.
    fn duplicate(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        let Some(store) = self.store.as_mut() else { return };
        let mut copy = config.clone();
        copy.id = String::new();
        copy.name = format!("{} copy", config.name);
        copy.password = None;
        let saved = match store.upsert(copy) {
            Ok(saved) => saved,
            Err(e) => {
                self.notice = Some(format!("Couldn’t duplicate “{}”: {e}", config.name));
                cx.notify();
                return;
            }
        };
        self.reload_connections();
        let (secrets, from, to) = (self.secrets.clone(), config.id.clone(), saved.id.clone());
        cx.spawn(async move |this, cx| {
            // The keyring can block (or prompt), so off the UI thread; selected once it's copied.
            if let Some(secrets) = secrets {
                cx.background_executor()
                    .spawn(async move {
                        if let Ok(Some(password)) = secrets.password(&from) {
                            if let Err(e) = secrets::save_password(secrets.as_ref(), &to, Some(&password)) {
                                log::warn!("the copy’s password wasn’t saved: {e}");
                            }
                        }
                    })
                    .await;
            }
            this.update(cx, |this, cx| this.select_connection(saved.id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// Copies the connection's URL, its saved password included (like the macOS app).
    fn copy_url(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        let secrets = self.secrets.clone();
        cx.spawn(async move |_, cx| {
            let config = cx
                .background_executor()
                .spawn(async move {
                    match secrets {
                        Some(store) => secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config),
                        None => config,
                    }
                })
                .await;
            cx.update(|cx| copy(config.to_url(true), cx));
        })
        .detach();
    }

    // MARK: related rows

    /// Opens the rows a foreign key leads to, in a tab of their own after the current one.
    fn open_related(&mut self, key: TargetKey, related: RelatedRows, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.open.get(&key).cloned() else { return };
        let table = self
            .schema_cache
            .get(&key)
            .and_then(|schemas| schemas.iter().find(|s| s.name == related.schema))
            .and_then(|schema| schema.tables.iter().find(|t| t.name == related.table))
            .cloned()
            .unwrap_or_else(|| TableInfo::new(related.schema.clone(), related.table.clone()));
        let dialect = Dialect(connection.config().kind);
        if !related.columns.is_empty() {
            let filter = dialect.match_filter(&related.columns, &related.values);
            return self.open_filtered(key, connection, table, filter, window, cx);
        }
        // SQLite can reference a primary key without naming its columns: look them up.
        cx.spawn_in(window, async move |this, cx| {
            let columns: Vec<String> = match connection.describe_table(table.clone()).await {
                Ok(structure) => structure.columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect(),
                Err(e) => return log::warn!("couldn’t find {}’s primary key: {e}", table.name),
            };
            if columns.len() != related.values.len() {
                return log::warn!("{}’s primary key doesn’t match the foreign key", table.name);
            }
            let filter = dialect.match_filter(&columns, &related.values);
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

    /// A script ran DDL on `key`: its tables are listed again, in the background when shown.
    fn schema_changed(&mut self, key: &TargetKey, cx: &mut Context<Self>) {
        self.schema_cache.remove(key);
        if self.target().is_some_and(|(_, current)| current == *key) {
            self.load_schemas_with(true, true, cx);
        }
    }

    // MARK: importing

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.store.as_ref().map(|s| s.connections().to_vec()).unwrap_or_default();
        let import = cx.new(|cx| ImportDialog::new(existing, cx));
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
        let Some(store) = self.store.as_mut() else {
            dialog.update(cx, |d, cx| d.show_error("The connection store isn’t available.".into(), cx));
            return false;
        };
        let mut problems = Vec::new();
        let mut imported = 0;
        for mut config in configs {
            // Imported connections have no id yet, so each one is added (never replaces another).
            let password = config.password.take().filter(|p| !p.is_empty());
            let name = config.name.clone();
            match store.upsert(config) {
                Ok(saved) => {
                    imported += 1;
                    let Some(password) = password else { continue };
                    match &self.secrets {
                        Some(secrets) => {
                            if let Err(e) = secrets::save_password(secrets.as_ref(), &saved.id, Some(&password)) {
                                problems.push(format!("{name}: the password wasn’t saved ({e})"));
                            }
                        }
                        None => problems.push(format!("{name}: the password wasn’t saved (no keyring)")),
                    }
                }
                Err(e) => problems.push(format!("{name}: {e}")),
            }
        }
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
        self.load_schemas_with(false, false, cx);
    }

    /// Lists the selected target's tables. `refresh`: ignore what's cached (and list the databases
    /// again too); `quiet`: keep showing the current list until the new one arrives.
    fn load_schemas_with(&mut self, refresh: bool, quiet: bool, cx: &mut Context<Self>) {
        let Some((config, key)) = self.target() else { return };
        if refresh {
            self.schema_cache.remove(&key);
        }
        if let Some(schemas) = self.schema_cache.get(&key) {
            self.schemas = Load::Loaded(schemas.clone());
            self.schemas_task = None;
            cx.notify();
            return;
        }
        if !(quiet && matches!(self.schemas, Load::Loaded(_))) {
            self.schemas = Load::Loading;
        }
        let existing = self.open.get(&key).cloned();
        let secrets = self.secrets.clone();
        let list_databases = Self::browses_databases(&config) && (refresh || !self.databases.contains_key(&config.id));
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
                match result {
                    Ok(schemas) => {
                        this.open.insert(key.clone(), connection);
                        this.schema_cache.insert(key.clone(), schemas.clone());
                        this.schemas = Load::Loaded(schemas);
                    }
                    // A background refresh: keep the list it couldn't replace.
                    Err(e) if quiet && matches!(this.schemas, Load::Loaded(_)) => log::warn!("couldn’t list the tables again: {e}"),
                    Err(e) => this.schemas = Load::Failed(e.to_string()),
                }
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
        let view = TabView::Script(cx.new(|cx| ScriptTab::new(connection, name, target, history, window, cx)));
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
        };
        match view {
            TabView::Table(tab) => {
                let handle = handle.clone();
                cx.subscribe_in(tab, window, move |this, _, event, window, cx| handle(this, event, window, cx))
            }
            TabView::Script(tab) => cx.subscribe_in(tab, window, move |this, _, event, window, cx| handle(this, event, window, cx)),
        }
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
                let (target, history) = (self.target_label(&key), self.history_for(&key));
                TabView::Script(cx.new(|cx| {
                    let mut tab = ScriptTab::new(connection, name, target, history, window, cx);
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
                                let (connect, duplicate, copy_url, refresh) = (this.clone(), this.clone(), this.clone(), this.clone());
                                let (config, id) = (config.clone(), config.id.clone());
                                let import = this.clone();
                                let (connect_id, refresh_id) = (id.clone(), id.clone());
                                let (duplicate_config, url_config) = (config.clone(), config.clone());
                                let menu = if connected {
                                    menu.item(PopupMenuItem::new("Disconnect").on_click(move |_, _, cx| {
                                        connect.update(cx, |w, cx| w.disconnect(&connect_id, cx)).ok();
                                    }))
                                    .item(PopupMenuItem::new("Refresh").on_click(move |_, _, cx| {
                                        let id = refresh_id.clone();
                                        refresh.update(cx, |w, cx| {
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
                                .item(PopupMenuItem::new("Import from DBeaver…").on_click(move |_, window, cx| {
                                    import.update(cx, |w, cx| w.open_import(window, cx)).ok();
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
                h_flex().gap_1().child(h_flex().flex_1().min_w_0().child(title)).child(
                    Button::new("refresh-tables")
                        .ghost()
                        .small()
                        .icon(IconName::RefreshCw)
                        .disabled(matches!(self.schemas, Load::Loading))
                        .tooltip("Refresh")
                        .on_click(cx.listener(|this, _, _, cx| this.load_schemas_with(true, false, cx))),
                ).child(
                    Button::new("new-script")
                        .ghost()
                        .small()
                        .icon(IconName::SquareTerminal)
                        .disabled(!connected)
                        .tooltip(format!("New SQL Script ({})", crate::keys::shortcut("secondary-t")))
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
                // The tab pads its label on both sides but not the suffix, so the
                // button would sit far from the title and flush against the right
                // edge. Pull it into the label's padding and pad its right side.
                let close = div().ml(px(-8.)).pr_2().child(close);
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
