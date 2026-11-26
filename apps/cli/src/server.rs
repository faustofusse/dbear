//! `serve`: the JSON-RPC server. Every operation of the CLI lives here: one-shot commands are
//! clients of it, in-process (over an in-memory pipe) or over the socket of a running server.
//!
//! Shared by all clients: the [`Service`] (stores, keychain, OAuth grants, the cached metadata
//! connections and schema/catalog caches). Per client ([`Session`]): the connections scripts run
//! on, so script sessions, temp tables and `cancel` never cross clients, and dump/restore jobs.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use dbcore::dump::{CancelToken, Compression, DataStyle, DumpContent, DumpProgress, DumpScope, ProgressFn};
use dbcore::restore::RestoreProgress;
use dbcore::{Connection, ConnectionConfig, Error, PageCursor, TableInfo};
use dbservice::{unblock, DumpChoices, RunOptions, Service, Target, SCRIPT_ROW_LIMIT};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

use crate::rpc::{self, Incoming, RpcError, INTERNAL_ERROR, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR};
use crate::wire::*;

/// The store is re-read at most this often (another app may have added or edited connections).
const RELOAD_EVERY: Duration = Duration::from_secs(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn params<T: DeserializeOwned>(params: Json) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(RpcError::invalid_params)
}

fn to_json<T: Serialize>(value: T) -> Result<Json, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::new(INTERNAL_ERROR, e.to_string()))
}

/// How a client reaches the server; it changes what the end of its input means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// One client on stdin/stdout: at EOF, requests in flight (and jobs) still finish and answer.
    Stdio,
    /// A socket client: when it hangs up, what it runs is cancelled.
    Socket,
    /// A one-shot command without a server.
    InProcess,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Socket => "socket",
            Self::InProcess => "in-process",
        }
    }
}

/// What every client of the process shares.
pub struct Server {
    service: Service,
    transport: Transport,
    shutdown: watch::Sender<bool>,
    last_reload: Mutex<Option<Instant>>,
}

impl Server {
    pub fn new(service: Service, transport: Transport) -> Arc<Self> {
        Arc::new(Self { service, transport, shutdown: watch::channel(false).0, last_reload: Mutex::new(None) })
    }

    /// Resolves once `server.shutdown` was called (or [`stop`](Self::stop)).
    pub async fn stopped(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|stop| *stop).await;
    }

    pub fn stop(&self) {
        self.shutdown.send_replace(true);
    }

    /// Re-reads the store (at most every [`RELOAD_EVERY`] unless forced), dropping what's cached
    /// for connections that were edited or removed.
    async fn refresh_store(&self, force: bool) {
        {
            let mut last = lock(&self.last_reload);
            if !force && last.is_some_and(|t| t.elapsed() < RELOAD_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        if !self.service.has_store() {
            return;
        }
        let service = self.service.clone();
        unblock(move || {
            let before = service.connections();
            if service.reload().is_err() {
                return;
            }
            let after = service.connections();
            for old in &before {
                if after.iter().find(|c| c.id == old.id) != Some(old) {
                    service.forget(&old.id);
                }
            }
        })
        .await;
    }

    async fn find(&self, spec: &str) -> Result<ConnectionConfig, RpcError> {
        self.refresh_store(false).await;
        let (service, spec) = (self.service.clone(), spec.to_string());
        let found = unblock({
            let (service, spec) = (service.clone(), spec.clone());
            move || service.find(&spec)
        })
        .await;
        match found {
            Err(Error::InvalidConfig(_)) => {
                // Maybe saved a moment ago.
                self.refresh_store(true).await;
                Ok(unblock(move || service.find(&spec)).await?)
            }
            other => Ok(other?),
        }
    }

    async fn resolve(&self, on: &On) -> Result<Target, RpcError> {
        let config = self.find(&on.connection).await?;
        let (service, database) = (self.service.clone(), on.database.clone());
        Ok(unblock(move || service.target_for(&config, database.as_deref())).await)
    }

    /// The table `name` (in `schema`, else wherever it's listed: a default schema first; `a.b`
    /// is also read as schema `a`, table `b`).
    async fn table(&self, target: &Target, schema: Option<&str>, name: &str) -> Result<TableInfo, RpcError> {
        let schemas = self.service.schemas(target, false).await?;
        let tables = || schemas.iter().flat_map(|s| &s.tables);
        if let Some(schema) = schema {
            return Ok(self.service.table_info(target, schema, name));
        }
        let mut found: Vec<&TableInfo> = tables().filter(|t| t.name == name).collect();
        if found.is_empty() {
            found = tables().filter(|t| t.name.eq_ignore_ascii_case(name)).collect();
        }
        if found.is_empty() {
            if let Some((schema, table)) = name.split_once('.') {
                if let Some(t) = tables().find(|t| t.schema == schema && t.name == table) {
                    return Ok(t.clone());
                }
            }
            return Err(Error::TableNotFound(name.to_string()).into());
        }
        if found.len() > 1 {
            if let Some(t) = found.iter().find(|t| ["public", "main", "dbo"].contains(&t.schema.as_str())) {
                return Ok((*t).clone());
            }
            let names: Vec<String> = found.iter().map(|t| t.qualified_name()).collect();
            return Err(Error::InvalidConfig(format!("“{name}” is in several schemas: {}. Give a schema.", names.join(", "))).into());
        }
        Ok(found[0].clone())
    }
}

type After = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A method's result, and work that must only start once it's sent (a job's notifications).
struct Reply {
    result: Json,
    after: Option<After>,
}

impl From<Json> for Reply {
    fn from(result: Json) -> Self {
        Self { result, after: None }
    }
}

struct Running {
    serial: u64,
    id: Option<Json>,
    target: Target,
    connection: Arc<Connection>,
}

/// One client.
struct Session {
    server: Arc<Server>,
    out: mpsc::UnboundedSender<String>,
    /// The client's own connection per target, with the config it was made from.
    own: tokio::sync::Mutex<HashMap<Target, (ConnectionConfig, Arc<Connection>)>>,
    running: Mutex<Vec<Running>>,
    jobs: Mutex<HashMap<String, CancelToken>>,
    serial: AtomicU64,
}

impl Session {
    fn send(&self, line: String) {
        let _ = self.out.send(line);
    }

    fn notify(&self, method: &str, params: impl Serialize) {
        if let Ok(params) = serde_json::to_value(params) {
            self.send(rpc::notification(method, &params));
        }
    }

    /// The client's own connection to `target`: a session of its own. Made again when the saved
    /// connection was edited since.
    async fn own_connection(&self, target: &Target) -> Result<Arc<Connection>, RpcError> {
        let current = self.server.service.target_config(target)?;
        let mut own = self.own.lock().await;
        if let Some((config, connection)) = own.get(target) {
            if *config == current {
                return Ok(connection.clone());
            }
        }
        let connection = self.server.service.new_connection(target).await?;
        if let Some((_, old)) = own.insert(target.clone(), (current, connection.clone())) {
            tokio::spawn(async move { old.disconnect().await });
        }
        Ok(connection)
    }

    /// Stops what the client runs (when `cancel`) and closes its sessions.
    async fn close(&self, cancel: bool) {
        if cancel {
            let running: Vec<Arc<Connection>> = lock(&self.running).iter().map(|r| r.connection.clone()).collect();
            for connection in running {
                connection.cancel().await;
            }
            for token in lock(&self.jobs).values() {
                token.cancel();
            }
        }
        let own: Vec<Arc<Connection>> = self.own.lock().await.drain().map(|(_, (_, c))| c).collect();
        for connection in own {
            connection.disconnect().await;
        }
    }

    fn job_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.serial.fetch_add(1, Ordering::Relaxed) + 1)
    }

    async fn call(self: &Arc<Self>, id: Option<&Json>, method: &str, p: Json) -> Result<Reply, RpcError> {
        let server = &self.server;
        let service = &server.service;
        Ok(match method {
            "server.info" => to_json(Info {
                name: "dbear-cli".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: rpc::PROTOCOL_VERSION,
                pid: std::process::id(),
                transport: server.transport.name().into(),
                store_path: service.store_path().map(Path::to_path_buf),
                problems: service.problems().to_vec(),
            })?
            .into(),
            "server.shutdown" => {
                let server = server.clone();
                Reply { result: json!({}), after: Some(Box::pin(async move { server.stop() })) }
            }
            "connections.list" => {
                server.refresh_store(true).await;
                to_json(ConnectionList { connections: service.connections().into_iter().map(Listed::new).collect() })?.into()
            }
            "connections.get" => {
                let p: ConnectionParams = params(p)?;
                let config = server.find(&p.connection).await?;
                let target = server.resolve(&On::new(&config.id, None)).await?;
                to_json(ConnectionDetail { listed: Listed::new(config), target })?.into()
            }
            "connections.url" => {
                let p: UrlParams = params(p)?;
                let config = server.find(&p.connection).await?;
                let service = service.clone();
                let url = unblock(move || service.url(&config.id, p.with_password)).await?;
                to_json(UrlResult { url })?.into()
            }
            "databases.list" => {
                let p: ConnectionParams = params(p)?;
                let config = server.find(&p.connection).await?;
                let current = server.resolve(&On::new(&config.id, None)).await?.database;
                let default = config.default_database().to_string();
                let databases = if config.supports_multiple_databases() {
                    service.connection(&Target::default_for(&config)).await?.list_databases().await?
                } else {
                    vec![default.clone()]
                };
                to_json(Databases { databases, default, current })?.into()
            }
            "schemas.list" => {
                let p: SchemasParams = params(p)?;
                let mut target = server.resolve(&p.on).await?;
                let mut loaded = service.load(&target, p.refresh).await?;
                if loaded.database_gone && p.on.database.is_none() {
                    // The remembered database was dropped (and is now forgotten): use the connection's own.
                    target = server.resolve(&p.on).await?;
                    loaded = service.load(&target, false).await?;
                }
                to_json(Schemas { target, schemas: loaded.schemas })?.into()
            }
            "columns.list" => {
                let p: On = params(p)?;
                let target = server.resolve(&p).await?;
                let tables = service.connection(&target).await?.list_columns().await?;
                to_json(ColumnsList { target, tables })?.into()
            }
            "table.describe" => {
                let p: TableParams = params(p)?;
                let target = server.resolve(&p.on).await?;
                let table = server.table(&target, p.schema.as_deref(), &p.table).await?;
                let structure = service.connection(&target).await?.describe_table(table.clone()).await?;
                to_json(Described { target, table, structure })?.into()
            }
            "table.rows" => {
                let p: RowsParams = params(p)?;
                let target = server.resolve(&p.table.on).await?;
                let table = server.table(&target, p.table.schema.as_deref(), &p.table.table).await?;
                let cursor = p.cursor.as_deref().map(PageCursor::decode).transpose()?;
                let page = service.connection(&target).await?.fetch_page(table.clone(), p.query, p.limit.max(1), cursor).await?;
                to_json(Rows { target, table, page })?.into()
            }
            "table.editable" => {
                let p: TableParams = params(p)?;
                let target = server.resolve(&p.on).await?;
                let table = server.table(&target, p.schema.as_deref(), &p.table).await?;
                let (editing, _) = service.table_editing(&target, &table).await?;
                to_json(Editable { target, editing })?.into()
            }
            "table.changes.preview" => {
                let p: ChangesParams = params(p)?;
                let target = server.resolve(&p.table.on).await?;
                let table = server.table(&target, p.table.schema.as_deref(), &p.table.table).await?;
                let statements = service.preview_table_changes(&target, &table, &p.changes).await?;
                to_json(ChangesPreview { target, table, statements })?.into()
            }
            "table.changes.apply" => {
                let p: ChangesParams = params(p)?;
                let target = server.resolve(&p.table.on).await?;
                let table = server.table(&target, p.table.schema.as_deref(), &p.table.table).await?;
                let rows_affected = service.apply_table_changes(&target, &table, p.changes, p.reviewed.as_deref()).await?;
                to_json(ChangesApplied { target, table, rows_affected })?.into()
            }
            "table.related" => {
                let p: RelatedParams = params(p)?;
                let target = server.resolve(&p.table.on).await?;
                let table = server.table(&target, p.table.schema.as_deref(), &p.table.table).await?;
                let connection = service.connection(&target).await?;
                let filter = service.related_filter(&connection, &table, &p.columns, &p.values).await?;
                to_json(Related { target, table, filter })?.into()
            }
            "query.run" => {
                let p: RunParams = params(p)?;
                let target = server.resolve(&p.on).await?;
                let connection = self.own_connection(&target).await?;
                let serial = self.serial.fetch_add(1, Ordering::Relaxed);
                lock(&self.running).push(Running { serial, id: id.cloned(), target: target.clone(), connection: connection.clone() });
                let max_rows = match p.max_rows {
                    None => Some(SCRIPT_ROW_LIMIT),
                    Some(None) | Some(Some(0)) => None,
                    Some(rows) => rows,
                };
                let options = RunOptions { max_rows, record_history: p.record_history, describe_sources: p.describe_sources };
                let outcome = service.run_script(&target, &connection, p.sql, options).await;
                lock(&self.running).retain(|r| r.serial != serial);
                to_json(RunResult { target, outcome })?.into()
            }
            "query.cancel" => {
                let p: CancelParams = params(p)?;
                let target = match &p.connection {
                    Some(connection) => Some(server.resolve(&On { connection: connection.clone(), database: p.database.clone() }).await?),
                    None => None,
                };
                let matching: Vec<Arc<Connection>> = lock(&self.running)
                    .iter()
                    .filter(|r| p.id.as_ref().is_none_or(|id| r.id.as_ref() == Some(id)))
                    .filter(|r| target.as_ref().is_none_or(|t| *t == r.target))
                    .map(|r| r.connection.clone())
                    .collect();
                for connection in &matching {
                    connection.cancel().await;
                }
                to_json(CancelResult { cancelled: matching.len() })?.into()
            }
            "complete" => {
                let p: CompleteParams = params(p)?;
                let target = server.resolve(&p.on).await?;
                let completions = service.complete(&target, &p.sql, p.offset).await?;
                to_json(CompleteResult { target, completions })?.into()
            }
            "history.list" => {
                let p: HistoryParams = params(p)?;
                let config = server.find(&p.connection).await?;
                let service = service.clone();
                let entries = unblock(move || service.history(&config.id, p.limit)).await?;
                to_json(History { entries })?.into()
            }
            "dump.start" => self.start_dump(params(p)?).await?,
            "restore.start" => self.start_restore(params(p)?).await?,
            "job.cancel" => {
                let p: JobParams = params(p)?;
                let token = lock(&self.jobs).get(&p.job).cloned();
                if let Some(token) = &token {
                    token.cancel();
                }
                to_json(JobCancelled { cancelled: token.is_some() })?.into()
            }
            "disconnect" => {
                let p: DisconnectParams = params(p)?;
                let id = match &p.connection {
                    Some(spec) => Some(server.find(spec).await?.id),
                    None => None,
                };
                let mut ids: Vec<String> = Vec::new();
                let own: Vec<Arc<Connection>> = {
                    let mut own = self.own.lock().await;
                    let targets: Vec<Target> = own.keys().filter(|t| id.as_ref().is_none_or(|id| t.connection_id == *id)).cloned().collect();
                    targets.into_iter().filter_map(|t| own.remove(&t).map(|(_, c)| (t, c))).map(|(t, c)| (ids.push(t.connection_id), c).1).collect()
                };
                for connection in own {
                    connection.disconnect().await;
                }
                match &id {
                    Some(id) => {
                        if service.open_connections().iter().any(|(t, _)| t.connection_id == *id) {
                            ids.push(id.clone());
                        }
                        service.disconnect_connection(id).await;
                    }
                    None => ids.extend(service.disconnect_all().await),
                }
                ids.sort();
                ids.dedup();
                to_json(Disconnected { connections: ids })?.into()
            }
            _ => return Err(RpcError::new(METHOD_NOT_FOUND, format!("Unknown method “{method}”."))),
        })
    }

    async fn start_dump(self: &Arc<Self>, p: DumpParams) -> Result<Reply, RpcError> {
        let server = &self.server;
        let target = server.resolve(&p.on).await?;
        let config = server.service.target_config(&target)?;
        let compression = p.compression.unwrap_or_else(|| match &p.path {
            Some(path) if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gz")) => Compression::Gzip,
            _ => Compression::None,
        });
        let path = absolute(p.path.unwrap_or_else(|| dbservice::default_dump_file_name(&config, compression).into()));
        let choices = DumpChoices {
            content: p.content.unwrap_or(DumpContent::SchemaAndData),
            compression,
            data_style: p.data_style.unwrap_or(DataStyle::Copy),
            drop_objects: p.drop_objects,
            create_database: p.create_database,
        };
        let options = dbservice::dump_options(config.kind, choices, p.scope.unwrap_or(DumpScope::Database));
        let job = self.job_id("dump");
        let token = CancelToken::new();
        lock(&self.jobs).insert(job.clone(), token.clone());
        let started = to_json(JobStarted { job: job.clone(), target: target.clone(), path: path.clone() })?;
        let session = self.clone();
        let after = async move {
            let progress: ProgressFn<DumpProgress> = {
                let (session, job) = (session.clone(), job.clone());
                Arc::new(move |p: &DumpProgress| session.notify("dump.progress", Progress { job: job.clone(), progress: p, fraction: p.fraction() }))
            };
            let result = session.server.service.dump(&target, path, options, progress, token).await;
            lock(&session.jobs).remove(&job);
            let result = match result {
                Ok(summary) => Outcome::Ok(summary),
                Err(e) => Outcome::Error(e),
            };
            session.notify("dump.finished", Finished { job, result });
        };
        Ok(Reply { result: started, after: Some(Box::pin(after)) })
    }

    async fn start_restore(self: &Arc<Self>, p: RestoreParams) -> Result<Reply, RpcError> {
        let server = &self.server;
        let target = server.resolve(&p.on).await?;
        let config = server.service.target_config(&target)?;
        let path = absolute(p.path);
        if !path.is_file() {
            return Err(Error::InvalidConfig(format!("{} isn’t a file.", path.display())).into());
        }
        let defaults = dbservice::default_restore_options(config.kind);
        let options = dbservice::restore_options(
            config.kind,
            p.single_transaction.unwrap_or(defaults.single_transaction),
            p.stop_on_error.unwrap_or(defaults.stop_on_error),
        );
        let job = self.job_id("restore");
        let token = CancelToken::new();
        lock(&self.jobs).insert(job.clone(), token.clone());
        let started = to_json(JobStarted { job: job.clone(), target: target.clone(), path: path.clone() })?;
        let session = self.clone();
        let after = async move {
            let progress: ProgressFn<RestoreProgress> = {
                let (session, job) = (session.clone(), job.clone());
                Arc::new(move |p: &RestoreProgress| session.notify("restore.progress", Progress { job: job.clone(), progress: p, fraction: p.fraction() }))
            };
            let result = session.server.service.restore(&target, path, options, progress, token).await;
            lock(&session.jobs).remove(&job);
            let result = match result {
                Ok(summary) => Outcome::Ok(summary),
                Err(e) => Outcome::Error(e),
            };
            session.notify("restore.finished", Finished { job, result });
        };
        Ok(Reply { result: started, after: Some(Box::pin(after)) })
    }

    /// Answers one line. Requests run concurrently (each on a task of its own).
    fn handle(self: &Arc<Self>, line: &[u8], tasks: &mut tokio::task::JoinSet<()>) {
        let message: Incoming = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(e) => {
                let error = if serde_json::from_slice::<Json>(line).is_ok() {
                    RpcError::new(INVALID_REQUEST, format!("Not a JSON-RPC message: {e}"))
                } else {
                    RpcError::new(PARSE_ERROR, format!("Not JSON: {e}"))
                };
                return self.send(rpc::response(&Json::Null, Err(error)));
            }
        };
        let Some(method) = message.method else {
            if message.result.is_none() && message.error.is_none() {
                self.send(rpc::response(&message.id.unwrap_or(Json::Null), Err(RpcError::new(INVALID_REQUEST, "No method."))));
            }
            // Responses from the client: the server sends no requests, nothing to do.
            return;
        };
        let session = self.clone();
        tasks.spawn(async move {
            let id = message.id;
            let reply = session.call(id.as_ref(), &method, message.params.unwrap_or(Json::Null)).await;
            let after = match reply {
                Ok(reply) => {
                    if let Some(id) = &id {
                        session.send(rpc::response(id, Ok(reply.result)));
                    }
                    reply.after
                }
                Err(error) => {
                    if let Some(id) = &id {
                        session.send(rpc::response(id, Err(error)));
                    }
                    None
                }
            };
            // Jobs report as notifications after their `*.start` answer.
            if let Some(after) = after {
                after.await;
            }
        });
    }
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|d| d.join(&path)).unwrap_or(path)
    }
}

/// Serves one client on `read`/`write` until it hangs up or the server stops.
pub async fn serve_stream<R, W>(server: Arc<Server>, transport: Transport, read: R, write: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let mut writer = tokio::spawn(async move {
        let mut write = write;
        while let Some(line) = rx.recv().await {
            let sent = async {
                write.write_all(line.as_bytes()).await?;
                write.write_all(b"\n").await?;
                write.flush().await
            };
            if sent.await.is_err() {
                break;
            }
        }
    });
    let session = Arc::new(Session {
        server: server.clone(),
        out,
        own: Default::default(),
        running: Default::default(),
        jobs: Default::default(),
        serial: AtomicU64::new(0),
    });
    let mut read = BufReader::new(read);
    let mut tasks = tokio::task::JoinSet::new();
    let mut line = Vec::new();
    let stopped = server.stopped();
    tokio::pin!(stopped);
    let mut stopping = false;
    let mut writer_done = false;
    loop {
        line.clear();
        tokio::select! {
            n = read.read_until(b'\n', &mut line) => match n {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !line.trim_ascii().is_empty() {
                        session.handle(line.trim_ascii(), &mut tasks);
                    }
                }
            },
            _ = &mut stopped => {
                stopping = true;
                break;
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    if transport != Transport::Stdio || stopping {
        session.close(true).await;
        // Give the answers to the cancelled work a moment to go out.
        let _ = tokio::time::timeout(Duration::from_secs(2), async { while tasks.join_next().await.is_some() {} }).await;
        tasks.abort_all();
    } else {
        // End of input: what's in flight still finishes and answers, unless the client is gone.
        tokio::select! {
            _ = async { while tasks.join_next().await.is_some() {} } => {}
            _ = &mut writer => {
                writer_done = true;
                session.close(true).await;
                tasks.abort_all();
            }
        }
        session.close(false).await;
    }
    drop(session);
    if !writer_done {
        // Ends once every answer is out (the last sender is dropped).
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    }
}
