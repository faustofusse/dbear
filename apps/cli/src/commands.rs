//! The one-shot commands: each sends the `serve` method that does the work (to a running server,
//! or to an in-process session) and formats its result.

use std::io::Write;
use std::path::{Path, PathBuf};

use dbcore::dump::{DataStyle, DumpContent, DumpProgress, DumpScope, DumpSummary, Compression};
use dbcore::restore::{RestoreProgress, RestoreSummary};
use dbcore::{DatabaseKind, QueryResult, RowQuery, SortKey, TableInfo, Value};
use dbservice::{Options, Service};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use crate::client::{decode, Client};
use crate::render::{self, column, optional, text, Format};
use crate::rpc::PROTOCOL_VERSION;
use crate::server::{serve_stream, Server, Transport};
use crate::wire::{self, *};
use crate::{socket, Command, Failure, Output, CANCELLED, FAILED, USAGE};

fn print(s: &str) -> Result<(), Failure> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(s.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(value).map_err(|e| Failure::new(FAILED, e.to_string()))?;
    print(&format!("{text}\n"))
}

fn open_service() -> Result<Service, Failure> {
    let service = Service::open(Options::default())?;
    for problem in service.problems() {
        eprintln!("dbear-cli: warning: {}", problem.error);
    }
    Ok(service)
}

/// A running server for this connection store when there is one, else an in-process session.
async fn connect(no_server: bool, verbose: bool) -> Result<Client, Failure> {
    if !no_server && cfg!(unix) {
        if let Some(path) = socket::default_path().filter(|p| p.exists()) {
            if let Ok(client) = Client::connect(&path).await {
                match client.call::<Info>("server.info", json!({})).await {
                    Ok(info) if info.protocol != PROTOCOL_VERSION => {
                        eprintln!("dbear-cli: ignoring the server on {}: protocol {} (this is {PROTOCOL_VERSION})", path.display(), info.protocol)
                    }
                    Ok(info) if !matches!((&info.store_path, socket::store_path()), (Some(a), Some(b)) if socket::same_file(a, &b)) => {
                        eprintln!("dbear-cli: ignoring the server on {}: it serves another connection store", path.display())
                    }
                    Ok(info) => {
                        if verbose {
                            eprintln!("dbear-cli: using the server on {} (pid {})", path.display(), info.pid);
                        }
                        return Ok(client);
                    }
                    Err(_) => {}
                }
            }
        }
    }
    if verbose {
        eprintln!("dbear-cli: working in-process");
    }
    let service = dbservice::unblock(open_service).await?;
    Ok(Client::in_process(service))
}

pub async fn run(command: Command, no_server: bool, verbose: bool) -> Result<(), Failure> {
    if let Command::Serve { socket } = command {
        return serve(socket).await;
    }
    // Read before connecting: a usage error shouldn't wait on a server.
    let sql = match &command {
        Command::Query { sql, file, .. } => Some(read_sql(sql.as_deref(), file.as_deref())?),
        _ => None,
    };
    let client = connect(no_server, verbose).await?;
    match command {
        Command::Ls { out } => ls(&client, &out).await,
        Command::Dbs { conn, out } => dbs(&client, &conn, &out).await,
        Command::Tables { on, schema, refresh, out } => tables(&client, &on, schema.as_deref(), refresh, &out).await,
        Command::Describe { on, table, schema, ddl, out } => describe(&client, &on, &table, schema, ddl, &out).await,
        Command::Rows { on, table, schema, filter, sort, limit, cursor, out } => {
            let query = RowQuery { sort: sort.iter().map(|s| sort_key(s)).collect(), filter };
            let params = RowsParams { table: TableParams { on: on_params(&on), schema, table }, query, limit, cursor };
            rows(&client, params, &out).await
        }
        Command::Query { on, max_rows, history, out, .. } => {
            let params = RunParams {
                on: on_params(&on),
                sql: sql.unwrap_or_default(),
                max_rows: Some(Some(max_rows)),
                record_history: history,
                describe_sources: false,
            };
            query(&client, params, &out).await
        }
        Command::Dump { on, file, schema_only, data_only, gzip, inserts, drop, create_database, schemas, tables, json } => {
            let scope = if !tables.is_empty() {
                let tables = tables.iter().map(|t| table_ref(t)).collect::<Result<Vec<_>, _>>()?;
                Some(DumpScope::Tables(tables))
            } else if !schemas.is_empty() {
                Some(DumpScope::Schemas(schemas))
            } else {
                None
            };
            let params = DumpParams {
                on: on_params(&on),
                path: file.map(absolute),
                content: Some(if schema_only {
                    DumpContent::SchemaOnly
                } else if data_only {
                    DumpContent::DataOnly
                } else {
                    DumpContent::SchemaAndData
                }),
                compression: gzip.then_some(Compression::Gzip),
                data_style: Some(if inserts { DataStyle::Insert } else { DataStyle::Copy }),
                drop_objects: drop,
                create_database,
                scope,
            };
            dump(&client, params, json).await
        }
        Command::Restore { on, file, no_transaction, continue_on_error, json } => {
            let params = RestoreParams {
                on: on_params(&on),
                path: absolute(file),
                single_transaction: no_transaction.then_some(false),
                stop_on_error: continue_on_error.then_some(false),
            };
            restore(&client, params, json).await
        }
        Command::History { conn, limit, out } => history(&client, &conn, limit, &out).await,
        Command::Url { conn, with_password } => {
            let url: UrlResult = client.call("connections.url", UrlParams { connection: conn, with_password }).await?;
            print(&format!("{}\n", url.url))
        }
        Command::Serve { .. } => unreachable!(),
    }
}

fn on_params(on: &crate::On) -> wire::On {
    wire::On::new(&on.conn, on.database.as_deref())
}

/// Paths go to a server that may run elsewhere: make them absolute here.
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|d| d.join(&path)).unwrap_or(path)
    }
}

fn sort_key(spec: &str) -> SortKey {
    if let Some(column) = spec.strip_prefix('-') {
        return SortKey { column: column.into(), descending: true };
    }
    match spec.rsplit_once(':') {
        Some((column, dir)) if dir.eq_ignore_ascii_case("desc") => SortKey { column: column.into(), descending: true },
        Some((column, dir)) if dir.eq_ignore_ascii_case("asc") => SortKey { column: column.into(), descending: false },
        _ => SortKey { column: spec.into(), descending: false },
    }
}

fn table_ref(spec: &str) -> Result<TableInfo, Failure> {
    match spec.split_once('.') {
        Some((schema, name)) => Ok(TableInfo::new(schema, name)),
        None => Err(Failure::new(USAGE, format!("--table {spec}: give it as schema.name (e.g. public.{spec}, main.{spec})"))),
    }
}

fn read_sql(sql: Option<&str>, file: Option<&Path>) -> Result<String, Failure> {
    let stdin = || -> Result<String, Failure> {
        use std::io::{IsTerminal, Read};
        if file.is_none() && sql.is_none() && std::io::stdin().is_terminal() {
            return Err(Failure::new(USAGE, "no SQL: give it as an argument, with -f FILE, or on stdin"));
        }
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(|e| Failure::new(FAILED, format!("stdin: {e}")))?;
        Ok(text)
    };
    let text = match (sql, file) {
        (Some("-"), _) => stdin()?,
        (Some(sql), _) => sql.to_string(),
        (None, Some(path)) if path == Path::new("-") => stdin()?,
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| Failure::new(FAILED, format!("{}: {e}", path.display())))?,
        (None, None) => stdin()?,
    };
    if text.trim().is_empty() {
        return Err(Failure::new(USAGE, "no SQL to run"));
    }
    Ok(text)
}

/// Prints `columns`/`rows` in the format asked for.
fn grid(out: &Output, kind: DatabaseKind, columns: &[dbcore::ColumnInfo], rows: &[Vec<Value>]) -> Result<Format, Failure> {
    let format = render::pick(out.format);
    print(&render::grid(format, kind, columns, rows, &render::style(out.max_width)))?;
    Ok(format)
}

async fn call_json<T: DeserializeOwned + Serialize>(client: &Client, method: &str, params: impl Serialize, out: &Output) -> Result<Option<T>, Failure> {
    let value = client.request(method, params).1.await?;
    if out.json {
        print_json(&value)?;
        return Ok(None);
    }
    Ok(Some(decode(value)?))
}

async fn ls(client: &Client, out: &Output) -> Result<(), Failure> {
    let Some(list) = call_json::<ConnectionList>(client, "connections.list", json!({}), out).await? else { return Ok(()) };
    let columns = ["name", "kind", "group", "summary", "id"].map(column);
    let rows: Vec<Vec<Value>> = list
        .connections
        .iter()
        .map(|c| {
            let config = &c.config;
            vec![text(&config.name), text(config.kind.display_name()), text(&config.group), text(&c.summary), text(&config.id)]
        })
        .collect();
    grid(out, DatabaseKind::Sqlite, &columns, &rows)?;
    Ok(())
}

async fn dbs(client: &Client, conn: &str, out: &Output) -> Result<(), Failure> {
    let Some(list) = call_json::<Databases>(client, "databases.list", ConnectionParams { connection: conn.into() }, out).await? else { return Ok(()) };
    let rows: Vec<Vec<Value>> = list.databases.iter().map(|d| vec![text(d)]).collect();
    grid(out, DatabaseKind::Sqlite, &[column("database")], &rows)?;
    Ok(())
}

async fn tables(client: &Client, on: &crate::On, schema: Option<&str>, refresh: bool, out: &Output) -> Result<(), Failure> {
    let params = SchemasParams { on: on_params(on), refresh };
    let Some(mut list) = call_json::<Schemas>(client, "schemas.list", params, out).await? else { return Ok(()) };
    if let Some(schema) = schema {
        list.schemas.retain(|s| s.name == schema);
        if list.schemas.is_empty() {
            return Err(Failure::new(FAILED, format!("no schema “{schema}” in {}", list.target.database)));
        }
    }
    let columns = vec![column("schema"), column("name"), column("kind"), dbcore::ColumnInfo { type_name: "bigint".into(), ..column("rows") }];
    let rows: Vec<Vec<Value>> = list
        .schemas
        .iter()
        .flat_map(|s| &s.tables)
        .map(|t| {
            let kind = serde_json::to_value(t.kind).ok().and_then(|k| k.as_str().map(str::to_string)).unwrap_or_default();
            vec![text(&t.schema), text(&t.name), text(kind), t.estimated_row_count.map_or(Value::Null, |n| Value::Int(n as i64))]
        })
        .collect();
    grid(out, DatabaseKind::Sqlite, &columns, &rows)?;
    Ok(())
}

async fn describe(client: &Client, on: &crate::On, table: &str, schema: Option<String>, ddl: bool, out: &Output) -> Result<(), Failure> {
    let params = TableParams { on: on_params(on), schema, table: table.into() };
    let Some(described) = call_json::<Described>(client, "table.describe", params, out).await? else { return Ok(()) };
    let structure = &described.structure;
    if ddl {
        return match &structure.ddl {
            Some(ddl) => print(&format!("{}\n", ddl.trim_end())),
            None => Err(Failure::new(FAILED, format!("no CREATE statement for {}", described.table.qualified_name()))),
        };
    }
    let columns = ["column", "type", "nullable", "default", "key", "comment"].map(column);
    let rows: Vec<Vec<Value>> = structure
        .columns
        .iter()
        .map(|c| {
            vec![
                text(&c.name),
                text(&c.type_name),
                text(if c.is_nullable { "yes" } else { "no" }),
                text(c.default_value.clone().unwrap_or_default()),
                text(if c.is_primary_key { "PK" } else { "" }),
                text(c.comment.clone().unwrap_or_default()),
            ]
        })
        .collect();
    let format = render::pick(out.format);
    if format != Format::Table {
        grid(out, DatabaseKind::Sqlite, &columns, &rows)?;
        return Ok(());
    }
    let style = render::style(out.max_width);
    let bold = |s: &str| if style.color { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() };
    let kind = serde_json::to_value(described.table.kind).ok().and_then(|k| k.as_str().map(str::to_string)).unwrap_or_default();
    let mut text_out = format!("{} ({kind})\n\n", bold(&described.table.qualified_name()));
    text_out.push_str(&render::table(&columns, &rows, &style));
    if !structure.indexes.is_empty() {
        text_out.push_str(&format!("\n{}\n", bold("Indexes:")));
        for index in &structure.indexes {
            let what = if index.is_primary { " PRIMARY KEY" } else if index.is_unique { " UNIQUE" } else { "" };
            text_out.push_str(&format!("  {}{what} ({})\n", index.name, index.columns.join(", ")));
        }
    }
    if !structure.foreign_keys.is_empty() {
        text_out.push_str(&format!("\n{}\n", bold("Foreign keys:")));
        for fk in &structure.foreign_keys {
            let name = if fk.name.is_empty() { String::new() } else { format!("{} ", fk.name) };
            let target = if fk.referenced_columns.is_empty() { String::new() } else { format!(" ({})", fk.referenced_columns.join(", ")) };
            text_out.push_str(&format!(
                "  {name}({}) → {}.{}{target} on update {} on delete {}\n",
                fk.columns.join(", "),
                fk.referenced_schema,
                fk.referenced_table,
                fk.on_update,
                fk.on_delete
            ));
        }
    }
    if !structure.referenced_by.is_empty() {
        text_out.push_str(&format!("\n{}\n", bold("Referenced by:")));
        for r in &structure.referenced_by {
            let name = if r.name.is_empty() { String::new() } else { format!(" {}", r.name) };
            text_out.push_str(&format!("  {}.{}{name} ({})\n", r.schema, r.table, r.columns.join(", ")));
        }
    }
    print(&text_out)
}

/// The table footer: `(3 rows, 12 ms)`.
fn footer(result: &QueryResult, extra: Option<String>) -> String {
    let mut parts = vec![render::count(result.rows.len() as u64, "row", "rows")];
    parts.extend(extra);
    format!("({})\n", parts.join(", "))
}

async fn rows(client: &Client, params: RowsParams, out: &Output) -> Result<(), Failure> {
    let Some(rows) = call_json::<Rows>(client, "table.rows", params, out).await? else { return Ok(()) };
    let kind = client_kind(client, &rows.target.connection_id).await;
    let result = &rows.page.result;
    let format = render::pick(out.format);
    print(&render::grid(format, kind, &result.columns, &result.rows, &render::style(out.max_width)))?;
    if format == Format::Table {
        let more = rows.page.next.as_ref().map(|_| "more: --limit or --json for the cursor".to_string());
        let total = result.total_count.map(|t| format!("{t} in all"));
        print(&footer(result, total.into_iter().chain(more).reduce(|a, b| format!("{a}, {b}"))))?;
    }
    Ok(())
}

/// The connection's kind, for `INSERT` quoting (SQLite's rules when it can't tell).
async fn client_kind(client: &Client, id: &str) -> DatabaseKind {
    client.call::<ConnectionDetail>("connections.get", ConnectionParams { connection: id.into() }).await.map_or(DatabaseKind::Sqlite, |d| d.listed.config.kind)
}

async fn query(client: &Client, params: RunParams, out: &Output) -> Result<(), Failure> {
    let (id, response) = client.request("query.run", &params);
    tokio::pin!(response);
    let value = tokio::select! {
        r = &mut response => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("dbear-cli: cancelling…");
            let _ = client.request("query.cancel", CancelParams { id: Some(json!(id)), ..Default::default() }).1.await;
            tokio::select! {
                r = &mut response => r,
                _ = tokio::signal::ctrl_c() => return Err(Failure::new(CANCELLED, "Query cancelled")),
            }
        }
    }?;
    let run: RunResult = decode(value.clone())?;
    if out.json {
        print_json(&value)?;
        return match run.outcome.result {
            Ok(_) => Ok(()),
            Err(e) => Err(Failure::quiet(Failure::from(e).code)),
        };
    }
    let result = run.outcome.result?;
    let format = render::pick(out.format);
    let took = format!("{} ms", run.outcome.duration.as_millis());
    if result.columns.is_empty() {
        let message = match result.rows_affected {
            Some(n) => format!("({}, {took})\n", render::count(n, "row affected", "rows affected")),
            None => format!("(done, {took})\n"),
        };
        if format == Format::Table {
            print(&message)?;
        } else {
            eprint!("{message}");
        }
        return Ok(());
    }
    let kind = client_kind(client, &run.target.connection_id).await;
    print(&render::grid(format, kind, &result.columns, &result.rows, &render::style(out.max_width)))?;
    if format == Format::Table {
        print(&footer(&result, Some(took)))?;
    }
    if result.truncated {
        let total = result.total_count.map(|t| format!(" of {t}")).unwrap_or_default();
        eprintln!("dbear-cli: showing the first {}{total} rows; --max-rows 0 shows all", result.rows.len());
    }
    Ok(())
}

async fn history(client: &Client, conn: &str, limit: usize, out: &Output) -> Result<(), Failure> {
    let params = HistoryParams { connection: conn.into(), limit };
    let Some(history) = call_json::<History>(client, "history.list", params, out).await? else { return Ok(()) };
    let columns = vec![
        column("ran_at"),
        column("database"),
        dbcore::ColumnInfo { type_name: "bigint".into(), ..column("ms") },
        dbcore::ColumnInfo { type_name: "bigint".into(), ..column("rows") },
        column("error"),
        column("sql"),
    ];
    let rows: Vec<Vec<Value>> = history
        .entries
        .iter()
        .map(|e| {
            vec![
                text(render::utc(e.ran_at)),
                text(&e.database),
                e.duration_ms.map_or(Value::Null, |d| Value::Int(d as i64)),
                e.rows.map_or(Value::Null, |r| Value::Int(r as i64)),
                optional(e.error.clone()),
                text(e.sql.split_whitespace().collect::<Vec<_>>().join(" ")),
            ]
        })
        .collect();
    grid(out, DatabaseKind::Sqlite, &columns, &rows)?;
    Ok(())
}

/// One updating line on a terminal, else a line now and then.
struct Status {
    tty: bool,
    last: String,
}

impl Status {
    fn new() -> Self {
        Self { tty: render::stderr_is_tty(), last: String::new() }
    }

    /// `key` decides when a non-terminal gets a new line (a phase or table change).
    fn show(&mut self, key: &str, line: &str) {
        if self.tty {
            eprint!("\r\x1b[K{line}");
        } else if key != self.last {
            eprintln!("{line}");
        }
        self.last = key.to_string();
    }

    fn clear(&self) {
        if self.tty && !self.last.is_empty() {
            eprint!("\r\x1b[K");
        }
    }
}

/// Runs a `*.start` job to its `*.finished` notification, showing its progress. Ctrl-C cancels it.
async fn job<P: DeserializeOwned, S: DeserializeOwned>(
    client: &Client,
    kind: &str,
    params: impl Serialize,
    mut show: impl FnMut(&P, Option<f64>, &mut Status),
) -> Result<(JobStarted, S), Failure> {
    let mut notifications = client.notifications();
    let started: JobStarted = client.call(&format!("{kind}.start"), params).await?;
    let mut status = Status::new();
    let mut cancelling = false;
    let (progress, finished) = (format!("{kind}.progress"), format!("{kind}.finished"));
    loop {
        tokio::select! {
            note = notifications.recv() => {
                let Some(note) = note else {
                    status.clear();
                    return Err(Failure::new(FAILED, "the server went away"));
                };
                if note.params.get("job").and_then(|j| j.as_str()) != Some(&started.job) {
                    continue;
                }
                if note.method == progress {
                    if let Ok(p) = serde_json::from_value::<Progress<P>>(note.params) {
                        show(&p.progress, p.fraction, &mut status);
                    }
                } else if note.method == finished {
                    status.clear();
                    let finished: Finished<S> = decode(note.params)?;
                    return match finished.result {
                        Outcome::Ok(summary) => Ok((started, summary)),
                        Outcome::Error(e) => Err(e.into()),
                    };
                }
            }
            _ = tokio::signal::ctrl_c(), if !cancelling => {
                cancelling = true;
                status.clear();
                eprintln!("dbear-cli: cancelling…");
                let _ = client.request("job.cancel", JobParams { job: started.job.clone() }).1.await;
            }
        }
    }
}

fn percent(fraction: Option<f64>) -> String {
    fraction.map(|f| format!("{:.0}% · ", f * 100.0)).unwrap_or_default()
}

async fn dump(client: &Client, params: DumpParams, as_json: bool) -> Result<(), Failure> {
    let (started, summary): (JobStarted, DumpSummary) = job(client, "dump", params, |p: &DumpProgress, fraction, status| {
        let phase = serde_json::to_value(p.phase).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let object = p.object.as_deref().map(|o| format!(" {o}")).unwrap_or_default();
        let line = format!(
            "Dumping · {}{phase}{object} · {}/{} tables · {} · {}",
            percent(fraction),
            p.tables_done,
            p.tables_total,
            render::count(p.rows_done, "row", "rows"),
            render::bytes(p.bytes_written)
        );
        status.show(&format!("{phase}{object}"), &line);
    })
    .await?;
    for warning in &summary.warnings {
        eprintln!("dbear-cli: warning: {warning}");
    }
    if as_json {
        return print_json(&json!({"job": started.job, "target": started.target, "path": started.path, "summary": summary}));
    }
    eprintln!(
        "Dumped {}, {} to {} ({}).",
        render::count(summary.tables as u64, "table", "tables"),
        render::count(summary.rows, "row", "rows"),
        started.path.display(),
        render::bytes(summary.bytes)
    );
    Ok(())
}

async fn restore(client: &Client, params: RestoreParams, as_json: bool) -> Result<(), Failure> {
    let (started, summary): (JobStarted, RestoreSummary) = job(client, "restore", params, |p: &RestoreProgress, fraction, status| {
        let line = format!(
            "Restoring · {}{} · {}",
            percent(fraction),
            render::count(p.statements, "statement", "statements"),
            render::count(p.errors as u64, "error", "errors")
        );
        status.show(&format!("{}", p.statements / 1000), &line);
    })
    .await?;
    for warning in &summary.warnings {
        eprintln!("dbear-cli: warning: {warning}");
    }
    for error in &summary.errors {
        eprintln!("dbear-cli: {error}");
    }
    if as_json {
        print_json(&json!({"job": started.job, "target": started.target, "path": started.path, "summary": summary}))?;
    } else {
        eprintln!(
            "Restored {} from {}{}.",
            render::count(summary.statements, "statement", "statements"),
            started.path.display(),
            if summary.rows > 0 { format!(" ({} copied)", render::count(summary.rows, "row", "rows")) } else { String::new() }
        );
    }
    if summary.error_count > 0 {
        return Err(Failure::new(FAILED, format!("{} failed", render::count(summary.error_count as u64, "statement", "statements"))));
    }
    Ok(())
}

async fn serve(socket: Option<Option<PathBuf>>) -> Result<(), Failure> {
    let service = dbservice::unblock(open_service).await?;
    let Some(path) = socket else {
        let server = Server::new(service, Transport::Stdio);
        serve_stream(server, Transport::Stdio, tokio::io::stdin(), tokio::io::stdout()).await;
        return Ok(());
    };
    serve_socket(service, path).await
}

#[cfg(unix)]
async fn serve_socket(service: Service, path: Option<PathBuf>) -> Result<(), Failure> {
    let path = path.or_else(socket::default_path).ok_or_else(|| Failure::new(FAILED, "no folder for the socket: give --socket PATH"))?;
    let bound = socket::bind(&path).await.map_err(|e| Failure::new(FAILED, format!("{}: {e}", path.display())))?;
    eprintln!("dbear-cli: serving on {}", path.display());
    let server = Server::new(service.clone(), Transport::Socket);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = bound.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let (read, write) = stream.into_split();
                    tokio::spawn(serve_stream(server.clone(), Transport::Socket, read, write));
                }
                Err(e) => eprintln!("dbear-cli: accept: {e}"),
            },
            _ = server.stopped() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    // Ends the client sessions (they cancel what they run), then the socket file goes.
    server.stop();
    drop(bound);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    service.disconnect_all().await;
    eprintln!("dbear-cli: stopped");
    Ok(())
}

#[cfg(not(unix))]
async fn serve_socket(_service: Service, _path: Option<PathBuf>) -> Result<(), Failure> {
    Err(Failure::new(USAGE, "--socket isn’t supported on this platform yet (Unix only): use stdio"))
}
