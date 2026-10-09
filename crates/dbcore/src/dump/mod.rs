//! Database dumps: plain SQL (optionally gzipped) that `psql`, the `mysql` client and `sqlite3`
//! can load, and so can [`crate::restore`].
//!
//! Every engine (Postgres, MySQL, SQLite, Turso / libSQL, SQL Server) has its own module that opens a dedicated connection, reads in one consistent
//! snapshot and writes raw wire values (never the lossy grid [`crate::Value`]). Output streams to
//! `<path>.partial` through a writer thread ([`writer`]) and is renamed when complete, so a
//! cancelled or failed dump never leaves a file that looks valid.
//!
//! New engines: add a module and an arm in [`run`].

pub(crate) mod libsql;
mod literal;
mod mysql;
mod order;
mod postgres;
mod sqlite;
mod sqlserver;
mod writer;

pub use literal::sqlite_float;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind, TableInfo};

/// What goes into the dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DumpContent {
    #[default]
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

impl DumpContent {
    pub fn schema(self) -> bool {
        self != Self::DataOnly
    }

    pub fn data(self) -> bool {
        self != Self::SchemaOnly
    }
}

/// Which objects to dump. Schemas are Postgres and SQL Server schemas; a MySQL connection dumps
/// its one database, and SQLite and Turso their `main` database, whatever schema names are given.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DumpScope {
    #[default]
    Database,
    Schemas(Vec<String>),
    /// Only these tables/views, plus what belongs to them (sequences, indexes, triggers, used types).
    Tables(Vec<TableInfo>),
}

impl DumpScope {
    fn includes_schema(&self, schema: &str) -> bool {
        match self {
            Self::Database => true,
            Self::Schemas(schemas) => schemas.iter().any(|s| s == schema),
            Self::Tables(tables) => tables.iter().any(|t| t.schema == schema),
        }
    }

    fn includes_table(&self, schema: &str, name: &str) -> bool {
        match self {
            Self::Tables(tables) => tables.iter().any(|t| t.schema == schema && t.name == name),
            _ => self.includes_schema(schema),
        }
    }

    /// Whole schemas (functions, types… are dumped), not just some tables.
    fn whole_schemas(&self) -> bool {
        !matches!(self, Self::Tables(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    #[default]
    None,
    Gzip,
}

/// How rows are written. `Copy` is Postgres only (other engines always use INSERTs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataStyle {
    #[default]
    Copy,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpOptions {
    pub content: DumpContent,
    pub scope: DumpScope,
    pub compression: Compression,
    pub data_style: DataStyle,
    /// `DROP … IF EXISTS` before each `CREATE`.
    pub drop_objects: bool,
    /// MySQL: `CREATE DATABASE IF NOT EXISTS` + `USE`, so the dump restores into the same name.
    pub create_database: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpPhase {
    Connecting,
    /// Types, tables, functions…
    Schema,
    /// Table rows.
    Data,
    /// Indexes, constraints, views, triggers.
    PostData,
    Finishing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DumpProgress {
    pub phase: DumpPhase,
    /// The table (or other object) being written, e.g. `public.users`.
    pub object: Option<String>,
    pub tables_done: u32,
    pub tables_total: u32,
    /// Rows written so far (all tables).
    pub rows_done: u64,
    /// Rows of the current table, and the planner's estimate of its size (when known).
    pub table_rows_done: u64,
    pub table_rows_estimate: Option<u64>,
    /// Uncompressed SQL produced so far.
    pub bytes_written: u64,
}

impl DumpProgress {
    /// How far along, 0…1, when it can tell (tables done, plus the current one's share by its
    /// estimated size).
    pub fn fraction(&self) -> Option<f64> {
        if self.tables_total == 0 {
            return None;
        }
        let mut done = f64::from(self.tables_done);
        if let Some(estimate) = self.table_rows_estimate.filter(|&e| e > 0) {
            if self.tables_done < self.tables_total {
                done += (self.table_rows_done as f64 / estimate as f64).min(1.0);
            }
        }
        Some((done / f64::from(self.tables_total)).min(1.0))
    }
}

/// The database a dump or restore works on, for titles: a SQLite file's name, else the database
/// (or the connection's name when it has none).
pub fn target_name(config: &ConnectionConfig) -> String {
    if config.kind == DatabaseKind::Sqlite {
        return crate::paths::file_name(&config.database).to_string();
    }
    if config.database.is_empty() { config.name.clone() } else { config.database.clone() }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpSummary {
    pub tables: u32,
    pub rows: u64,
    /// Size of the file on disk (compressed when gzipped).
    pub bytes: u64,
    /// Objects that were skipped or may not restore exactly.
    pub warnings: Vec<String>,
}

/// Progress callback, called at most ~10 times a second (and once per phase/table change).
pub type ProgressFn<P> = Arc<dyn Fn(&P) + Send + Sync>;

/// Cancels a running dump or restore. Cheap to clone; cancelling is immediate: the work is
/// dropped, which closes its connection (the server stops the query) and deletes partial output.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once [`CancelToken::cancel`] is called.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Default file name: `app_dev-2025-06-01.sql` (`.sql.gz` when gzipped).
pub fn default_file_name(database: &str, date: &str, compression: Compression) -> String {
    let base: String = database
        .rsplit('/')
        .next()
        .unwrap_or(database)
        .trim_end_matches(".sqlite")
        .trim_end_matches(".sqlite3")
        .trim_end_matches(".db")
        .chars()
        .map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect();
    let base = if base.is_empty() { "dump".to_string() } else { base };
    let ext = if compression == Compression::Gzip { "sql.gz" } else { "sql" };
    format!("{base}-{date}.{ext}")
}

/// Dumps the database `config` points at into `path`. The file only appears once the dump is
/// complete; on error or cancel nothing is left behind.
pub async fn dump(
    config: ConnectionConfig,
    path: PathBuf,
    options: DumpOptions,
    progress: ProgressFn<DumpProgress>,
    cancel: CancelToken,
) -> Result<DumpSummary> {
    crate::connection::on_runtime(run(config, path, options, progress, cancel)).await
}

async fn run(
    config: ConnectionConfig,
    path: PathBuf,
    options: DumpOptions,
    progress: ProgressFn<DumpProgress>,
    cancel: CancelToken,
) -> Result<DumpSummary> {
    if crate::mock::is_mock(&config) {
        return Err(Error::Unsupported("sample connections can’t be dumped".into()));
    }
    // Through an SSH server: one tunnel for the whole dump.
    let (_tunnel, config) = crate::ssh::route(config).await?;
    let (out, writer) = writer::start(&path, options.compression)?;
    let mut ctx = Ctx::new(out, options, progress);
    ctx.report();

    let result = {
        let engine = engine(&config, &mut ctx);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            r = engine => r,
        }
    };
    let result = match result {
        Ok(()) => {
            ctx.phase(DumpPhase::Finishing);
            ctx.out.finish().await
        }
        Err(e) => Err(e),
    };
    if result.is_err() {
        ctx.out.abort();
    }
    let written = writer.await.map_err(|e| Error::Internal(e.to_string()))?;
    result?;
    let bytes = written?;
    Ok(DumpSummary { tables: ctx.tables_done, rows: ctx.progress.rows_done, bytes, warnings: ctx.warnings })
}

#[allow(unreachable_patterns)] // engines added on other branches fall back to `Unsupported` until they get a dumper
async fn engine(config: &ConnectionConfig, ctx: &mut Ctx) -> Result<()> {
    match config.kind {
        DatabaseKind::Postgres => postgres::dump(config, ctx).await,
        DatabaseKind::Mysql => mysql::dump(config, ctx).await,
        DatabaseKind::Sqlite => sqlite::dump(config, ctx).await,
        DatabaseKind::Libsql => libsql::dump(config, ctx).await,
        DatabaseKind::SqlServer => sqlserver::dump(config, ctx).await,
        _ => Err(Error::Unsupported(format!("dumping {} databases", config.kind.display_name()))),
    }
}

/// State shared by the engines: output buffer, progress and warnings.
pub(crate) struct Ctx {
    pub out: writer::Out,
    pub options: DumpOptions,
    pub warnings: Vec<String>,
    pub tables_done: u32,
    progress: DumpProgress,
    sink: ProgressFn<DumpProgress>,
    last_report: Instant,
}

const REPORT_EVERY: Duration = Duration::from_millis(100);

impl Ctx {
    fn new(out: writer::Out, options: DumpOptions, sink: ProgressFn<DumpProgress>) -> Self {
        Self {
            out,
            options,
            warnings: Vec::new(),
            tables_done: 0,
            progress: DumpProgress {
                phase: DumpPhase::Connecting,
                object: None,
                tables_done: 0,
                tables_total: 0,
                rows_done: 0,
                table_rows_done: 0,
                table_rows_estimate: None,
                bytes_written: 0,
            },
            sink,
            last_report: Instant::now(),
        }
    }

    pub fn push(&mut self, s: &str) {
        self.out.push(s.as_bytes());
    }

    /// Writes `s` plus a newline.
    pub fn line(&mut self, s: &str) {
        self.out.push(s.as_bytes());
        self.out.push(b"\n");
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    pub fn phase(&mut self, phase: DumpPhase) {
        self.progress.phase = phase;
        self.progress.object = None;
        self.report();
    }

    pub fn set_tables_total(&mut self, total: u32) {
        self.progress.tables_total = total;
    }

    pub fn begin_table(&mut self, name: String, estimate: Option<u64>) {
        self.progress.object = Some(name);
        self.progress.table_rows_done = 0;
        self.progress.table_rows_estimate = estimate;
        self.report();
    }

    pub fn end_table(&mut self) {
        self.tables_done += 1;
        self.progress.tables_done = self.tables_done;
        self.report();
    }

    pub fn rows(&mut self, n: u64) {
        self.progress.rows_done += n;
        self.progress.table_rows_done += n;
    }

    /// Hands full buffers to the writer (waiting when it's behind) and reports progress.
    /// Call often (e.g. per row): it's cheap when there's nothing to do.
    pub async fn tick(&mut self) -> Result<()> {
        self.out.flush_if_full().await?;
        if self.last_report.elapsed() >= REPORT_EVERY {
            self.report();
        }
        Ok(())
    }

    fn report(&mut self) {
        self.progress.bytes_written = self.out.bytes();
        self.last_report = Instant::now();
        (self.sink)(&self.progress);
    }
}

/// Header comment shared by every engine.
pub(crate) fn header(ctx: &mut Ctx, config: &ConnectionConfig, server: &str) {
    ctx.line(&format!("-- dbear {} dump of “{}”", config.kind.display_name(), config.default_database()));
    if !server.is_empty() {
        ctx.line(&format!("-- Server version: {server}"));
    }
    let content = match ctx.options.content {
        DumpContent::SchemaAndData => "schema and data",
        DumpContent::SchemaOnly => "schema only",
        DumpContent::DataOnly => "data only",
    };
    ctx.line(&format!("-- Content: {content}"));
    ctx.line("");
}

/// For tests and the GPUI app: a path next to `path` used while writing.
pub fn partial_path(path: &Path) -> PathBuf {
    writer::partial_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fraction_counts_the_current_table() {
        let mut p = DumpProgress {
            phase: DumpPhase::Data,
            object: None,
            tables_done: 1,
            tables_total: 4,
            rows_done: 0,
            table_rows_done: 50,
            table_rows_estimate: Some(100),
            bytes_written: 0,
        };
        assert_eq!(p.fraction(), Some(0.375));
        p.table_rows_done = 500; // past the estimate: the table counts as done, no more
        assert_eq!(p.fraction(), Some(0.5));
        p.tables_total = 0;
        assert_eq!(p.fraction(), None);
    }

    #[test]
    fn names_targets() {
        let mut config = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
        config.database = "/home/me/notes.db".into();
        assert_eq!(target_name(&config), "notes.db");
        let mut config = ConnectionConfig::new_empty(DatabaseKind::Postgres);
        config.name = "Prod".into();
        assert_eq!(target_name(&config), "Prod");
        config.database = "app".into();
        assert_eq!(target_name(&config), "app");
    }

    #[test]
    fn names_files() {
        assert_eq!(default_file_name("app_dev", "2025-06-01", Compression::None), "app_dev-2025-06-01.sql");
        assert_eq!(default_file_name("/x/y/notes.db", "d", Compression::Gzip), "notes-d.sql.gz");
        assert_eq!(default_file_name("we ird/", "d", Compression::None), "dump-d.sql");
        assert_eq!(default_file_name("a b", "d", Compression::None), "a_b-d.sql");
    }

    #[test]
    fn scopes() {
        let tables = DumpScope::Tables(vec![TableInfo::new("public", "users")]);
        assert!(tables.includes_table("public", "users"));
        assert!(!tables.includes_table("public", "orders"));
        assert!(tables.includes_schema("public"));
        assert!(!tables.whole_schemas());
        let schemas = DumpScope::Schemas(vec!["billing".into()]);
        assert!(schemas.includes_table("billing", "x") && !schemas.includes_table("public", "x"));
    }

    #[tokio::test]
    async fn cancel_wakes_waiters() {
        let token = CancelToken::new();
        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });
        tokio::task::yield_now().await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        // Already cancelled: returns at once.
        token.cancelled().await;
    }
}
