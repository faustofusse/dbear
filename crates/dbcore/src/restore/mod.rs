//! Restoring SQL scripts: dbear dumps, and the plain output of `pg_dump`, `mysqldump` and
//! `sqlite3 .dump` (gzipped or not), and T-SQL scripts with `GO` batches (sqlcmd, SSMS).
//!
//! A blocking thread reads and splits the file ([`split`]) and sends statements to the engine's
//! runner, which executes them on a dedicated connection: `COPY … FROM stdin` blocks go through
//! Postgres' COPY protocol. Nothing is loaded whole, so big dumps restore in constant memory.

mod libsql;
mod mysql;
mod postgres;
pub mod split;
mod sqlite;
mod sqlserver;

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::sync::mpsc;

use crate::driver::{Error, Result};
use crate::dump::{CancelToken, ProgressFn};
use crate::model::{ConnectionConfig, DatabaseKind};
use split::{Item, Splitter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Run everything in one transaction (Postgres, SQLite): all or nothing. The script's own
    /// `BEGIN`/`COMMIT` are skipped. MySQL commits DDL implicitly, so it only helps data there.
    pub single_transaction: bool,
    /// Stop at the first error. Otherwise errors are collected and the rest still runs
    /// (always stops in a single transaction, which an error aborts).
    pub stop_on_error: bool,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self { single_transaction: true, stop_on_error: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreProgress {
    /// Bytes of the file read so far (compressed bytes for a gzipped file).
    pub bytes_read: u64,
    pub bytes_total: u64,
    pub statements: u64,
    pub errors: u32,
}

impl RestoreProgress {
    /// How far along, 0…1: the share of the file read.
    pub fn fraction(&self) -> Option<f64> {
        (self.bytes_total > 0).then(|| (self.bytes_read as f64 / self.bytes_total as f64).min(1.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreSummary {
    /// Statements (and COPY blocks) that ran.
    pub statements: u64,
    /// Rows loaded by COPY blocks.
    pub rows: u64,
    /// Failed statements, when not stopping on errors (the first 100, with their line).
    pub errors: Vec<String>,
    pub error_count: u32,
    /// Lines that were skipped (psql meta-commands…).
    pub warnings: Vec<String>,
}

/// Runs the script at `path` against the database `config` points at.
pub async fn restore(
    config: ConnectionConfig,
    path: PathBuf,
    options: RestoreOptions,
    progress: ProgressFn<RestoreProgress>,
    cancel: CancelToken,
) -> Result<RestoreSummary> {
    crate::connection::on_runtime(run(config, path, options, progress, cancel)).await
}

async fn run(
    config: ConnectionConfig,
    path: PathBuf,
    options: RestoreOptions,
    progress: ProgressFn<RestoreProgress>,
    cancel: CancelToken,
) -> Result<RestoreSummary> {
    if crate::mock::is_mock(&config) {
        return Err(Error::Unsupported("sample connections can’t be restored into".into()));
    }
    // Through an SSH server: one tunnel for the whole restore.
    let (_tunnel, config) = crate::ssh::route(config).await?;
    let total = std::fs::metadata(&path).map_err(|e| read_error(&path, &e))?.len();
    let read = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel(256);
    {
        let (path, read, kind) = (path.clone(), read.clone(), config.kind);
        // Stops by itself once the runner is gone (its sends fail).
        tokio::task::spawn_blocking(move || read_script(&path, kind, &tx, &read));
    }
    let tally = Tally::new(options, progress, read, total, cancel.clone());
    let runner = runner(config, rx, tally);
    let tally = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        r = runner => r?,
    };
    Ok(tally.finish())
}

#[allow(unreachable_patterns)] // engines added on other branches fall back to `Unsupported`
async fn runner(config: ConnectionConfig, rx: mpsc::Receiver<Result<Item>>, tally: Tally) -> Result<Tally> {
    match config.kind {
        DatabaseKind::Postgres => postgres::run(&config, rx, tally).await,
        DatabaseKind::Mysql => mysql::run(&config, rx, tally).await,
        DatabaseKind::Sqlite => sqlite::run(&config, rx, tally).await,
        DatabaseKind::Libsql => libsql::run(&config, rx, tally).await,
        DatabaseKind::SqlServer => sqlserver::run(&config, rx, tally).await,
        _ => Err(Error::Unsupported(format!("restoring into {} databases", config.kind.display_name()))),
    }
}

fn read_error(path: &Path, e: &std::io::Error) -> Error {
    Error::Query(format!("Couldn’t read {}: {e}", path.display()))
}

/// Counts the bytes read from the file, for progress.
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Opens a script, transparently gunzipping it.
pub fn open_script(path: &Path, read: Arc<AtomicU64>) -> Result<Box<dyn BufRead + Send>> {
    let file = File::open(path).map_err(|e| read_error(path, &e))?;
    let mut buffered = BufReader::with_capacity(256 * 1024, Counting { inner: file, read });
    let head = buffered.fill_buf().map_err(|e| read_error(path, &e))?;
    if head.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(BufReader::with_capacity(256 * 1024, flate2::bufread::MultiGzDecoder::new(buffered))))
    } else {
        Ok(Box::new(buffered))
    }
}

fn read_script(path: &Path, kind: DatabaseKind, tx: &mpsc::Sender<Result<Item>>, read: &Arc<AtomicU64>) {
    let send = |item: Result<Item>| tx.blocking_send(item).is_ok();
    let mut reader = match open_script(path, read.clone()) {
        Ok(reader) => reader,
        Err(e) => {
            send(Err(e));
            return;
        }
    };
    let mut splitter = Splitter::new(kind);
    let mut line = Vec::new();
    let mut items = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                send(Err(read_error(path, &e)));
                return;
            }
        }
        if splitter.in_copy() {
            splitter.push_copy_line(&line, &mut items);
        } else {
            let Ok(mut text) = std::str::from_utf8(&line) else {
                send(Err(Error::Query(format!("Line {}: not valid UTF-8 text", splitter.line() + 1))));
                return;
            };
            if splitter.line() == 0 {
                text = text.trim_start_matches('\u{feff}');
            }
            splitter.push_line(text, &mut items);
        }
        for item in items.drain(..) {
            if !send(Ok(item)) {
                return;
            }
        }
    }
    splitter.finish(&mut items);
    for item in items {
        if !send(Ok(item)) {
            return;
        }
    }
}

static TRANSACTION_CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(begin|start\s+transaction|commit|end)(\s+(transaction|work))?\s*;?$").unwrap()
});

const MAX_ERRORS_KEPT: usize = 100;
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// Progress, errors and the error policy, shared by the engine runners.
pub(crate) struct Tally {
    options: RestoreOptions,
    sink: ProgressFn<RestoreProgress>,
    read: Arc<AtomicU64>,
    total: u64,
    cancel: CancelToken,
    last_report: Instant,
    summary: RestoreSummary,
}

impl Tally {
    fn new(options: RestoreOptions, sink: ProgressFn<RestoreProgress>, read: Arc<AtomicU64>, total: u64, cancel: CancelToken) -> Self {
        let mut tally = Self { options, sink, read, total, cancel, last_report: Instant::now(), summary: RestoreSummary::default() };
        tally.report();
        tally
    }

    fn single_transaction(&self) -> bool {
        self.options.single_transaction
    }

    /// Errors end the restore (stop on error, or the transaction is lost anyway).
    fn stops_on_error(&self) -> bool {
        self.options.stop_on_error || self.options.single_transaction
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The script's own `BEGIN`/`COMMIT`, skipped inside our single transaction.
    fn skips(&self, sql: &str) -> bool {
        self.options.single_transaction && TRANSACTION_CONTROL.is_match(sql.trim())
    }

    fn ran(&mut self) {
        self.summary.statements += 1;
        if self.last_report.elapsed() >= REPORT_EVERY {
            self.report();
        }
    }

    fn rows(&mut self, n: u64) {
        self.summary.rows += n;
    }

    /// Records a failed statement, or fails the restore (stop on error, single transaction,
    /// lost connection, cancel).
    fn failed(&mut self, line: usize, e: Error) -> Result<()> {
        let message = match &e {
            Error::Cancelled | Error::ConnectionFailed(_) => return Err(e),
            Error::Query(m) => m.clone(),
            other => other.to_string(),
        };
        let message = format!("Line {line}: {message}");
        if self.options.stop_on_error || self.options.single_transaction {
            let note = if self.options.single_transaction { "\nNothing was restored." } else { "" };
            return Err(Error::Query(format!("{message}{note}")));
        }
        self.summary.error_count += 1;
        if self.summary.errors.len() < MAX_ERRORS_KEPT {
            self.summary.errors.push(message);
        }
        self.report();
        Ok(())
    }

    /// psql meta-commands: `\restrict`/`\unrestrict` (pg_dump 17.6+) are safe to skip; others are
    /// skipped with a warning, except `\connect`, which would restore into another database.
    fn meta(&mut self, command: &str, line: usize) -> Result<()> {
        let name = command.split_whitespace().next().unwrap_or(command);
        match name {
            "\\restrict" | "\\unrestrict" => Ok(()),
            "\\connect" | "\\c" => self.failed(
                line,
                Error::Query(format!("“{command}” switches to another database: restore into it directly instead")),
            ),
            _ => {
                if self.summary.warnings.len() < MAX_ERRORS_KEPT {
                    self.summary.warnings.push(format!("Line {line}: skipped psql command “{command}”"));
                }
                Ok(())
            }
        }
    }

    fn report(&mut self) {
        self.last_report = Instant::now();
        (self.sink)(&RestoreProgress {
            bytes_read: self.read.load(Ordering::Relaxed).min(self.total),
            bytes_total: self.total,
            statements: self.summary.statements,
            errors: self.summary.error_count,
        });
    }

    fn finish(mut self) -> RestoreSummary {
        self.report();
        self.summary
    }
}

/// `COPY … FROM stdin` blocks are Postgres only.
fn copy_unsupported(kind: DatabaseKind) -> Error {
    Error::Unsupported(format!("COPY … FROM stdin blocks are PostgreSQL only, not {}", kind.display_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fraction_is_the_share_read() {
        let p = RestoreProgress { bytes_read: 25, bytes_total: 100, statements: 0, errors: 0 };
        assert_eq!(p.fraction(), Some(0.25));
        assert_eq!(RestoreProgress { bytes_total: 0, ..p }.fraction(), None);
    }

    #[test]
    fn recognises_transaction_control() {
        for sql in ["BEGIN", "begin transaction", "COMMIT", "END", "start transaction", "commit work;"] {
            assert!(TRANSACTION_CONTROL.is_match(sql), "{sql}");
        }
        for sql in ["begin; select 1", "commit and chain", "BEGIN ATOMIC", "end loop"] {
            assert!(!TRANSACTION_CONTROL.is_match(sql), "{sql}");
        }
    }

    #[test]
    fn reads_gzipped_scripts() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sql.gz");
        let mut gz = flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::fast());
        gz.write_all(b"select 1;\n").unwrap();
        gz.finish().unwrap();
        let read = Arc::new(AtomicU64::new(0));
        let mut text = String::new();
        open_script(&path, read.clone()).unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "select 1;\n");
        assert_eq!(read.load(Ordering::Relaxed), std::fs::metadata(&path).unwrap().len());
    }
}
