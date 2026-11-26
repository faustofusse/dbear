//! The glue every dbear client needs around `dbcore`, written once: the GPUI app, the CLI and its
//! JSON-RPC server all go through [`Service`].
//!
//! - Opening the stores: the connection store (honours `DBEAR_CONNECTIONS_FILE`), `state.db` beside
//!   it, the OS keychain, and the Cloudflare OAuth write-back (`d1::oauth::sync_with_store`).
//! - Finding a connection by id, name or URL, and the database it opens ([`Target`]).
//! - Saved config + keychain secrets → [`Connection`], one cached per target.
//! - Schema, database list and completion catalog caches, invalidated by DDL, restores and edits.
//! - Connection CRUD with the store → keychain ordering, scripts with history, dump/restore helpers.
//!
//! Threading: `Service` is `Clone + Send + Sync`. Async methods are executor-agnostic like
//! `dbcore::Connection` (the work runs on dbcore's runtime or a blocking thread; any executor can
//! await them). Methods documented as **blocking** read the keychain (which can prompt) or SQLite
//! (up to 5 s busy timeout): call them off a UI thread, e.g. through [`unblock`].

mod backup;
mod crud;
mod editing;
mod script;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use dbcore::complete::{self, Catalog, Completions};
use dbcore::dialect::Dialect;
use dbcore::secrets::{self, SecretStore};
use dbcore::state::StateStore;
use dbcore::{Connection, ConnectionConfig, ConnectionStore, Error, Result, Schema, TableInfo, Value};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

pub use backup::{
    civil_date, default_dump_file_name, default_restore_options, dump_options, dump_scope, normalize_dump_options, normalize_restore_options,
    restore_options, today, DumpChoices,
};
pub use crud::{Import, SaveRequest, Saved, SecretKind, SecretProblem};
pub use editing::{EditableColumn, ForeignKeyRef, ReferenceRef, TableEditing};
pub use script::{history_entry, RunOptions, ScriptOutcome, SCRIPT_ROW_LIMIT};

/// The environment variable that points the apps (and this crate) at another connection store.
pub const CONNECTIONS_FILE_ENV: &str = "DBEAR_CONNECTIONS_FILE";

/// Prefix of the ids given to connections opened from a URL (they aren't saved).
pub const URL_ID_PREFIX: &str = "url:";

/// Set (to anything but empty or `0`) to keep every client away from the OS keychain:
/// [`Secrets::Keyring`] opens as [`Secrets::Disabled`]. For tests and sandboxes.
pub const NO_KEYRING_ENV: &str = "DBEAR_NO_KEYRING";

fn keyring_disabled() -> bool {
    std::env::var_os(NO_KEYRING_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Runs blocking work (keychain, SQLite, files) on a thread of its own and awaits it from any
/// executor. The work still finishes if the future is dropped.
pub async fn unblock<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("dbservice-blocking".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .expect("couldn’t start a thread");
    match rx.await {
        Ok(value) => value,
        Err(_) => panic!("blocking work panicked"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where passwords come from.
#[derive(Clone, Default)]
pub enum Secrets {
    /// The OS credential store (`KeyringSecretStore`): Keychain, Secret Service, Credential Manager.
    #[default]
    Keyring,
    /// Another store (tests use `MemorySecretStore`).
    Store(Arc<dyn SecretStore>),
    /// No passwords at all: connections use only what their config carries.
    Disabled,
}

/// How to open a [`Service`].
#[derive(Clone)]
pub struct Options {
    /// The connection store. `None`: `$DBEAR_CONNECTIONS_FILE`, else the default (`dbcore::store::default_path`).
    pub store_path: Option<PathBuf>,
    /// The state file (history, UI state). `None`: `state.db` beside the connection store.
    pub state_path: Option<PathBuf>,
    pub secrets: Secrets,
    /// Install `d1::oauth::sync_with_store` with the secret store, so rotated "Sign in with
    /// Cloudflare" grants are saved back (process-wide). Default true.
    pub oauth_sync: bool,
    /// With no saved connections, [`Service::listed_connections`] lists `dbcore::mock::connections()`
    /// and their ids resolve (the GPUI app's first launch). Default false.
    pub samples: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { store_path: None, state_path: None, secrets: Secrets::Keyring, oauth_sync: true, samples: false }
    }
}

/// Which part of the service didn't open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Part {
    /// The connection store: no saved connections (lenient open only).
    Store,
    /// The keychain: no saved passwords.
    Secrets,
    /// `state.db`: no history and no UI state.
    State,
}

/// Something that didn't open; the service works without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub part: Part,
    pub error: Error,
}

/// A connection and the database it opens: the key of every per-database cache. `database` is
/// always a name, the connection's `default_database()` when it's its own (history records it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Target {
    pub connection_id: String,
    pub database: String,
}

impl Target {
    pub fn new(connection_id: impl Into<String>, database: impl Into<String>) -> Self {
        Self { connection_id: connection_id.into(), database: database.into() }
    }

    /// The connection's own database.
    pub fn default_for(config: &ConnectionConfig) -> Self {
        Self::new(config.id.clone(), config.default_database())
    }
}

/// What [`Service::load`] found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Loaded {
    pub schemas: Vec<Schema>,
    /// The server's databases, when they were listed this time (connections that browse databases,
    /// on refresh or the first load). Also kept: [`Service::databases`].
    pub databases: Option<Vec<String>>,
    /// The target's database isn't on the server any more (dropped or renamed): its remembered
    /// `last_database` was cleared, and the caller should switch to the connection's own.
    pub database_gone: bool,
    /// Served from the schema cache, no round trip.
    pub cached: bool,
}

/// Whether the connection offers its server's other databases (title picker, remembered database).
pub fn browses_databases(config: &ConnectionConfig) -> bool {
    config.show_all_databases && config.supports_multiple_databases()
}

/// `config` pointed at `database` (unchanged when it's the connection's own).
pub fn pointed_at(config: &ConnectionConfig, database: &str) -> ConnectionConfig {
    if database == config.default_database() {
        config.clone()
    } else {
        config.with_database(database)
    }
}

#[derive(Default)]
struct Caches {
    open: HashMap<Target, Arc<Connection>>,
    schemas: HashMap<Target, Vec<Schema>>,
    catalogs: HashMap<Target, Arc<OnceCell<Arc<Catalog>>>>,
    databases: HashMap<String, Vec<String>>,
    /// Bumped by `forget`: work started before it must not fill the caches again.
    generations: HashMap<String, u64>,
}

impl Caches {
    fn generation(&self, id: &str) -> u64 {
        self.generations.get(id).copied().unwrap_or(0)
    }
}

struct Inner {
    store: Option<Mutex<ConnectionStore>>,
    store_path: Option<PathBuf>,
    /// The store's list as of the last read or write, so listing never waits on SQLite.
    listed: RwLock<Arc<Vec<ConnectionConfig>>>,
    state: Option<Mutex<StateStore>>,
    secrets: Option<Arc<dyn SecretStore>>,
    samples: bool,
    /// Connections opened from a URL (never saved), by their `url:` id.
    adhoc: Mutex<HashMap<String, ConnectionConfig>>,
    caches: Mutex<Caches>,
    problems: Vec<Problem>,
}

/// Shared by every client of the process. Cheap to clone.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").field("store", &self.inner.store_path).field("problems", &self.inner.problems).finish()
    }
}

fn open_store(path: Option<PathBuf>) -> Result<ConnectionStore> {
    match path.or_else(|| std::env::var_os(CONNECTIONS_FILE_ENV).map(PathBuf::from)) {
        Some(path) => ConnectionStore::open(path),
        None => ConnectionStore::open_default(),
    }
}

impl Service {
    /// Opens everything (**blocking**: SQLite, keychain). Fails when the connection store can't be
    /// opened; a missing keychain or state file only shows in [`problems`](Self::problems).
    pub fn open(options: Options) -> Result<Self> {
        let service = Self::open_lenient(options);
        match service.inner.problems.iter().find(|p| p.part == Part::Store) {
            Some(problem) => Err(problem.error.clone()),
            None => Ok(service),
        }
    }

    /// [`open`](Self::open) with the default options (the user's stores and keychain).
    pub fn open_default() -> Result<Self> {
        Self::open(Options::default())
    }

    /// Opens what it can (**blocking**) and never fails: whatever didn't open is in
    /// [`problems`](Self::problems) (the GPUI app still starts without a store, showing samples).
    pub fn open_lenient(options: Options) -> Self {
        let mut problems = Vec::new();
        let store = open_store(options.store_path).map_err(|error| problems.push(Problem { part: Part::Store, error })).ok();
        let secrets: Option<Arc<dyn SecretStore>> = match options.secrets {
            Secrets::Keyring if keyring_disabled() => None,
            Secrets::Keyring => match secrets::KeyringSecretStore::new() {
                Ok(store) => Some(Arc::new(store)),
                Err(error) => {
                    problems.push(Problem { part: Part::Secrets, error });
                    None
                }
            },
            Secrets::Store(store) => Some(store),
            Secrets::Disabled => None,
        };
        if let (true, Some(secrets)) = (options.oauth_sync, &secrets) {
            // Refreshed "Sign in with Cloudflare" grants go back to the connection's entry.
            dbcore::d1::oauth::sync_with_store(secrets.clone());
        }
        let state_path = options.state_path.or_else(|| store.as_ref().map(|s| s.path().with_file_name(dbcore::state::STATE_FILE)));
        let state = state_path.and_then(|path| StateStore::open(path).map_err(|error| problems.push(Problem { part: Part::State, error })).ok());
        let listed = store.as_ref().map(|s| s.connections().to_vec()).unwrap_or_default();
        let store_path = store.as_ref().map(|s| s.path().to_path_buf());
        Self {
            inner: Arc::new(Inner {
                store: store.map(Mutex::new),
                store_path,
                listed: RwLock::new(Arc::new(listed)),
                state: state.map(Mutex::new),
                secrets,
                samples: options.samples,
                adhoc: Mutex::new(HashMap::new()),
                caches: Mutex::new(Caches::default()),
                problems,
            }),
        }
    }

    /// What didn't open (store, keychain, state file), in that order.
    pub fn problems(&self) -> &[Problem] {
        &self.inner.problems
    }

    pub fn store_path(&self) -> Option<&Path> {
        self.inner.store_path.as_deref()
    }

    pub fn has_store(&self) -> bool {
        self.inner.store.is_some()
    }

    pub fn has_secrets(&self) -> bool {
        self.inner.secrets.is_some()
    }

    pub fn has_state(&self) -> bool {
        self.inner.state.is_some()
    }

    /// The secret store in use, for flows this crate doesn't cover (Cloudflare sign-in's saved grant).
    pub fn secret_store(&self) -> Option<Arc<dyn SecretStore>> {
        self.inner.secrets.clone()
    }

    fn store(&self) -> Result<MutexGuard<'_, ConnectionStore>> {
        match &self.inner.store {
            Some(store) => Ok(lock(store)),
            None => Err(Error::Storage("the connection store isn’t available".into())),
        }
    }

    fn state(&self) -> Option<MutexGuard<'_, StateStore>> {
        self.inner.state.as_ref().map(lock)
    }

    /// Copies the store's list into the snapshot. Call with the store locked, after a write or reload.
    fn publish(&self, store: &ConnectionStore) {
        *self.inner.listed.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(store.connections().to_vec());
    }

    // MARK: lookup

    /// Saved connections in the user's order (no passwords). Never touches SQLite.
    pub fn connections(&self) -> Vec<ConnectionConfig> {
        self.inner.listed.read().unwrap_or_else(|p| p.into_inner()).as_ref().clone()
    }

    /// What a connection list shows: the saved connections, or (with `Options::samples`) the
    /// samples while there are none. The flag says it's the samples.
    pub fn listed_connections(&self) -> (Vec<ConnectionConfig>, bool) {
        let saved = self.connections();
        if self.inner.samples && saved.is_empty() {
            (dbcore::mock::connections(), true)
        } else {
            (saved, false)
        }
    }

    /// Re-reads the saved connections from disk (another process may have changed them). **Blocking.**
    pub fn reload(&self) -> Result<()> {
        let mut store = self.store()?;
        store.reload()?;
        self.publish(&store);
        Ok(())
    }

    /// The connection `id`: saved, opened from a URL by [`find`](Self::find), or a sample (with
    /// `Options::samples`, while nothing is saved). No secrets.
    pub fn config(&self, id: &str) -> Option<ConnectionConfig> {
        if let Some(config) = self.inner.listed.read().unwrap_or_else(|p| p.into_inner()).iter().find(|c| c.id == id) {
            return Some(config.clone());
        }
        if let Some(config) = lock(&self.inner.adhoc).get(id) {
            return Some(config.clone());
        }
        let (listed, samples) = self.listed_connections();
        listed.into_iter().find(|c| samples && c.id == id)
    }

    /// A connection by id, exact name (case-insensitive; an error lists them when several match)
    /// or URL. A URL isn't saved: it gets a `url:` id (the same for the same URL) that resolves
    /// for the life of the service, with the URL's password kept in memory.
    pub fn find(&self, spec: &str) -> Result<ConnectionConfig> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(Error::InvalidConfig("No connection given.".into()));
        }
        if let Some(config) = self.config(spec) {
            return Ok(config);
        }
        let (listed, _) = self.listed_connections();
        let named: Vec<&ConnectionConfig> = listed.iter().filter(|c| c.name.trim().eq_ignore_ascii_case(spec)).collect();
        match named.as_slice() {
            [one] => return Ok((*one).clone()),
            [] => {}
            several => {
                let names: Vec<String> = several.iter().map(|c| format!("“{}” ({})", c.name, c.id)).collect();
                return Err(Error::InvalidConfig(format!(
                    "“{spec}” matches {} connections: {}. Use an id.",
                    several.len(),
                    names.join(", ")
                )));
            }
        }
        if spec.contains("://") || spec.starts_with("sqlite:") || spec.starts_with("file:") {
            let mut config = ConnectionConfig::from_url(spec)?;
            config.id = url_id(spec);
            lock(&self.inner.adhoc).insert(config.id.clone(), config.clone());
            return Ok(config);
        }
        Err(Error::InvalidConfig(format!("No saved connection is called “{spec}” (or has that id).")))
    }

    // MARK: targets

    /// The database last browsed on a connection that browses databases, unless it's its own.
    /// **Blocking** (reads the store, which another app may have written).
    pub fn remembered_database(&self, config: &ConnectionConfig) -> Option<String> {
        if !browses_databases(config) {
            return None;
        }
        self.store().ok()?.last_database(&config.id).filter(|db| db != config.default_database())
    }

    /// The target `config` opens on: `database` when given, else the remembered one (connections
    /// that browse databases), else its own. **Blocking** (see [`remembered_database`](Self::remembered_database)).
    pub fn target_for(&self, config: &ConnectionConfig, database: Option<&str>) -> Target {
        let database = match database.map(str::trim).filter(|d| !d.is_empty()) {
            Some(database) => database.to_string(),
            None => self.remembered_database(config).unwrap_or_else(|| config.default_database().to_string()),
        };
        Target::new(config.id.clone(), database)
    }

    /// [`find`](Self::find), then [`target_for`](Self::target_for). **Blocking.**
    pub fn resolve(&self, spec: &str, database: Option<&str>) -> Result<Target> {
        let config = self.find(spec)?;
        Ok(self.target_for(&config, database))
    }

    /// Remembers the database browsed on connection `id` (forgotten when it's the connection's own),
    /// for [`remembered_database`](Self::remembered_database). Shared with the macOS app. **Blocking.**
    pub fn set_last_database(&self, id: &str, database: &str) -> Result<()> {
        let Some(config) = self.config(id) else { return Ok(()) };
        let remembered = (database != config.default_database()).then_some(database);
        if self.inner.store.is_none() {
            return Ok(());
        }
        self.store()?.set_last_database(id, remembered)
    }

    /// The target's config, pointed at its database. No secrets.
    pub fn target_config(&self, target: &Target) -> Result<ConnectionConfig> {
        let config = self.config(&target.connection_id).ok_or_else(|| unknown(&target.connection_id))?;
        Ok(pointed_at(&config, &target.database))
    }

    /// `config` with its saved password and SSH secret (unless typed in already, or the tunnel uses
    /// the agent). A keychain that fails leaves them out, like the apps. **Blocking**.
    pub fn fill_secrets(&self, config: ConnectionConfig) -> ConnectionConfig {
        match &self.inner.secrets {
            Some(store) if !config.id.is_empty() && !config.id.starts_with(URL_ID_PREFIX) => {
                secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config)
            }
            _ => config,
        }
    }

    /// The target's config with its secrets, read on a blocking thread: for dumps, restores and
    /// anything else that opens its own session.
    pub async fn config_with_secrets(&self, target: &Target) -> Result<ConnectionConfig> {
        let config = self.target_config(target)?;
        let service = self.clone();
        Ok(unblock(move || service.fill_secrets(config)).await)
    }

    /// "conn · db" for a connection that browses databases and isn't on its own name, else its name.
    pub fn target_label(&self, target: &Target) -> String {
        match self.config(&target.connection_id) {
            Some(c) if browses_databases(&c) && c.name != target.database => format!("{} · {}", c.name, target.database),
            Some(c) => c.name,
            None => target.database.clone(),
        }
    }

    // MARK: connections

    /// A new, uncached connection to the target with its secrets: a session of its own (scripts,
    /// temp tables and `cancel` aren't shared). The JSON-RPC server opens one per client.
    pub async fn new_connection(&self, target: &Target) -> Result<Arc<Connection>> {
        let config = self.config_with_secrets(target).await?;
        Ok(Arc::new(Connection::new(config)))
    }

    /// The cached connection to the target, if one is open.
    pub fn cached(&self, target: &Target) -> Option<Arc<Connection>> {
        lock(&self.inner.caches).open.get(target).cloned()
    }

    /// The shared connection to the target, opened (and cached) on first use. It connects lazily:
    /// the first call on it reaches the server.
    pub async fn connection(&self, target: &Target) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(target) {
            return Ok(connection);
        }
        let generation = lock(&self.inner.caches).generation(&target.connection_id);
        let connection = self.new_connection(target).await?;
        Ok(self.keep(target, connection, generation))
    }

    /// The shared connection to the target, connected: the cached one as is, else a new one that
    /// is cached only once it connects (a target that fails leaves nothing behind, no status
    /// light). The GPUI app reopens last session's tabs with it.
    pub async fn connected(&self, target: &Target) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(target) {
            return Ok(connection);
        }
        let generation = lock(&self.inner.caches).generation(&target.connection_id);
        let connection = self.new_connection(target).await?;
        connection.connect().await?;
        Ok(self.keep(target, connection, generation))
    }

    /// Caches `connection` unless the connection was forgotten meanwhile or another call won.
    fn keep(&self, target: &Target, connection: Arc<Connection>, generation: u64) -> Arc<Connection> {
        let mut caches = lock(&self.inner.caches);
        if caches.generation(&target.connection_id) != generation {
            return connection;
        }
        caches.open.entry(target.clone()).or_insert(connection).clone()
    }

    /// Every cached connection with its target.
    pub fn open_connections(&self) -> Vec<(Target, Arc<Connection>)> {
        let mut open: Vec<_> = lock(&self.inner.caches).open.iter().map(|(t, c)| (t.clone(), c.clone())).collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        open
    }

    /// Ids of connections with a cached session that the server closed (none of its targets is
    /// still connected). They reconnect when next used. The apps poll this for their status light.
    pub async fn lost_connections(&self) -> HashSet<String> {
        let mut alive: HashMap<String, bool> = HashMap::new();
        for (target, connection) in self.open_connections() {
            let connected = connection.is_connected().await;
            *alive.entry(target.connection_id).or_default() |= connected;
        }
        alive.into_iter().filter(|(_, connected)| !connected).map(|(id, _)| id).collect()
    }

    /// Drops everything cached for connection `id` (it was edited, deleted or disconnected):
    /// connections, schemas, catalogs, database list. Returns the dropped connections so the
    /// caller can [`Connection::disconnect`] them (dropping them also closes them eventually).
    pub fn forget(&self, id: &str) -> Vec<Arc<Connection>> {
        let mut caches = lock(&self.inner.caches);
        *caches.generations.entry(id.to_string()).or_default() += 1;
        let dropped: Vec<Arc<Connection>> = caches.open.iter().filter(|(t, _)| t.connection_id == id).map(|(_, c)| c.clone()).collect();
        caches.open.retain(|t, _| t.connection_id != id);
        caches.schemas.retain(|t, _| t.connection_id != id);
        caches.catalogs.retain(|t, _| t.connection_id != id);
        caches.databases.remove(id);
        dropped
    }

    /// Forgets connection `id` and closes its sessions.
    pub async fn disconnect_connection(&self, id: &str) {
        for connection in self.forget(id) {
            connection.disconnect().await;
        }
    }

    /// Closes the target's cached session (others of the same connection stay open).
    pub async fn disconnect(&self, target: &Target) {
        let dropped = {
            let mut caches = lock(&self.inner.caches);
            caches.schemas.remove(target);
            caches.catalogs.remove(target);
            caches.open.remove(target)
        };
        if let Some(connection) = dropped {
            connection.disconnect().await;
        }
    }

    /// Forgets every open connection and closes them. Returns the ids that were open.
    pub async fn disconnect_all(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.inner.caches).open.keys().map(|t| t.connection_id.clone()).collect();
        ids.sort();
        ids.dedup();
        for id in &ids {
            self.disconnect_connection(id).await;
        }
        ids
    }

    /// Cancels what the target's cached connection is running (scripts on other connections, e.g.
    /// a JSON-RPC client's own, are cancelled on those).
    pub async fn cancel(&self, target: &Target) {
        if let Some(connection) = self.cached(target) {
            connection.cancel().await;
        }
    }

    // MARK: schemas and catalogs

    /// The target's schemas, if listed and still valid.
    pub fn cached_schemas(&self, target: &Target) -> Option<Vec<Schema>> {
        lock(&self.inner.caches).schemas.get(target).cloned()
    }

    /// The targets whose schemas are cached (listed and still valid), sorted.
    pub fn schema_targets(&self) -> Vec<Target> {
        let mut targets: Vec<Target> = lock(&self.inner.caches).schemas.keys().cloned().collect();
        targets.sort();
        targets
    }

    /// The databases on connection `id`'s server, once listed by [`load`](Self::load).
    pub fn databases(&self, id: &str) -> Option<Vec<String>> {
        lock(&self.inner.caches).databases.get(id).cloned()
    }

    /// Forgets connection `id`'s database list (a database was created or dropped).
    pub fn invalidate_databases(&self, id: &str) {
        lock(&self.inner.caches).databases.remove(id);
    }

    /// The tables may have changed (DDL ran, a restore finished): drops the target's schemas and
    /// completion catalog. The next [`load`](Self::load) lists them again.
    pub fn schema_changed(&self, target: &Target) {
        let mut caches = lock(&self.inner.caches);
        caches.schemas.remove(target);
        caches.catalogs.remove(target);
    }

    /// Lists the target's tables (the apps' sidebar load). `refresh` ignores the caches and lists
    /// the server's databases again too. The shared connection is reused, or opened and cached
    /// once listing works (a connection that fails isn't kept: the next try starts afresh). For a
    /// connection that browses databases, the databases are listed on the first load and on
    /// refresh; a remembered database that's gone is forgotten (see [`Loaded::database_gone`]).
    pub async fn load(&self, target: &Target, refresh: bool) -> Result<Loaded> {
        let config = self.config(&target.connection_id).ok_or_else(|| unknown(&target.connection_id))?;
        let (existing, generation, list_databases) = {
            let mut caches = lock(&self.inner.caches);
            if refresh {
                caches.schemas.remove(target);
                caches.catalogs.remove(target);
            } else if let Some(schemas) = caches.schemas.get(target) {
                return Ok(Loaded { schemas: schemas.clone(), databases: None, database_gone: false, cached: true });
            }
            let list_databases = browses_databases(&config) && (refresh || !caches.databases.contains_key(&config.id));
            (caches.open.get(target).cloned(), caches.generation(&target.connection_id), list_databases)
        };
        let connection = match existing {
            Some(connection) => connection,
            None => self.new_connection(target).await?,
        };
        let schemas = connection.list_schemas().await?;
        // Not fatal when it fails: the connection still works with its own database.
        let databases = if list_databases { connection.list_databases().await.ok() } else { None };
        {
            let mut caches = lock(&self.inner.caches);
            if caches.generation(&target.connection_id) == generation {
                caches.open.entry(target.clone()).or_insert(connection);
                caches.schemas.insert(target.clone(), schemas.clone());
                if let Some(databases) = &databases {
                    caches.databases.insert(config.id.clone(), databases.clone());
                }
            }
        }
        let database_gone = databases.as_ref().is_some_and(|d| !d.contains(&target.database)) && config.default_database() != target.database;
        if database_gone && self.inner.store.is_some() {
            let (service, id) = (self.clone(), config.id.clone());
            let _ = unblock(move || service.store().and_then(|mut s| s.set_last_database(&id, None))).await;
        }
        Ok(Loaded { schemas, databases, database_gone, cached: false })
    }

    /// The target's schemas (see [`load`](Self::load)).
    pub async fn schemas(&self, target: &Target, refresh: bool) -> Result<Vec<Schema>> {
        Ok(self.load(target, refresh).await?.schemas)
    }

    /// A table of the target as listed (kind, row estimate), or a plain table when it isn't.
    pub fn table_info(&self, target: &Target, schema: &str, name: &str) -> TableInfo {
        lock(&self.inner.caches)
            .schemas
            .get(target)
            .and_then(|schemas| schemas.iter().find(|s| s.name == schema))
            .and_then(|s| s.tables.iter().find(|t| t.name == name))
            .cloned()
            .unwrap_or_else(|| TableInfo::new(schema, name))
    }

    /// The target's completion catalog, if loaded and still valid.
    pub fn cached_catalog(&self, target: &Target) -> Option<Arc<Catalog>> {
        lock(&self.inner.caches).catalogs.get(target).and_then(|cell| cell.get().cloned())
    }

    /// The target's tables and columns for completion, listed once on the shared connection
    /// (concurrent callers share the listing) and kept until the schema changes.
    pub async fn catalog(&self, target: &Target) -> Result<Arc<Catalog>> {
        let cell = lock(&self.inner.caches).catalogs.entry(target.clone()).or_default().clone();
        let catalog = cell
            .get_or_try_init(|| async {
                let connection = self.connection(target).await?;
                let schemas = connection.list_schemas().await?;
                let columns = connection.list_columns().await?;
                Ok::<_, Error>(Arc::new(Catalog::new(schemas, columns)))
            })
            .await?;
        Ok(catalog.clone())
    }

    /// Completions at byte `offset` of `sql` on the target. Keywords still complete when the
    /// catalog can't be loaded.
    pub async fn complete(&self, target: &Target, sql: &str, offset: usize) -> Result<Completions> {
        let config = self.config(&target.connection_id).ok_or_else(|| unknown(&target.connection_id))?;
        let catalog = self.catalog(target).await.unwrap_or_else(|_| Arc::new(Catalog::default()));
        let offset = floor_char_boundary(sql, offset);
        Ok(complete::complete(sql, offset, &catalog, Dialect(config.kind)))
    }

    /// The `WHERE` filter for the rows a foreign key points at. With no columns (SQLite can
    /// reference a primary key without naming it), the table's primary key is looked up.
    pub async fn related_filter(&self, connection: &Connection, table: &TableInfo, columns: &[String], values: &[Value]) -> Result<String> {
        let dialect = Dialect(connection.config().kind);
        if !columns.is_empty() {
            return Ok(dialect.match_filter(columns, values));
        }
        let structure = connection.describe_table(table.clone()).await?;
        let key: Vec<String> = structure.columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
        if key.len() != values.len() {
            return Err(Error::Query(format!("{}’s primary key doesn’t match the foreign key", table.name)));
        }
        Ok(dialect.match_filter(&key, values))
    }

    // MARK: UI state

    /// A value kept under `key` in `state.db` (the GPUI app's `gpui.session`…). **Blocking.**
    pub fn state_get(&self, key: &str) -> Option<String> {
        self.state()?.get(key)
    }

    /// Sets (or with `None` removes) `key` in `state.db`. **Blocking.**
    pub fn state_set(&self, key: &str, value: Option<&str>) -> Result<()> {
        match self.state() {
            Some(mut state) => state.set(key, value),
            None => Err(Error::Storage("the state file (state.db) isn’t available".into())),
        }
    }
}

fn unknown(id: &str) -> Error {
    Error::InvalidConfig(format!("No connection with id “{id}”."))
}

/// Stable within a build: the same URL gets the same id (so its connection is reused).
fn url_id(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{URL_ID_PREFIX}{:016x}", hasher.finish())
}

fn floor_char_boundary(s: &str, mut offset: usize) -> usize {
    offset = offset.min(s.len());
    while !s.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn is<T: Send + Sync + Clone + 'static>() {}
    is::<Service>();
}
