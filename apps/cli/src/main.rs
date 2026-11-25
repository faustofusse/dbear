//! `dbear-cli`: dbear's saved connections (read-only), queries, dumps and restores from a shell, and
//! `serve`, a JSON-RPC server for editor plugins (see PROTOCOL.md). Everything goes through
//! `dbservice`, like the GPUI app: same store, keychain entries and history.

mod client;
mod commands;
mod render;
mod rpc;
mod server;
mod socket;
mod wire;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use render::Format;

/// Exit codes.
pub const FAILED: u8 = 1;
pub const USAGE: u8 = 2;
/// Ctrl-C (128 + SIGINT, as shells report it).
pub const CANCELLED: u8 = 130;

#[derive(Parser)]
#[command(name = "dbear-cli", version, about = "dbear's saved connections, queries and dumps from the command line.")]
#[command(after_help = "<CONN> is a saved connection's name or id, or a URL (postgres://…, mysql://…, sqlite:path…).\n\
Commands use a running `dbear-cli serve --socket` when there is one (its open connections are reused).\n\
Environment: DBEAR_CONNECTIONS_FILE (another connection store), DBEAR_SOCKET (the socket path),\n\
DBEAR_NO_KEYRING=1 (never read the keychain), NO_COLOR.")]
struct Cli {
    /// Work in this process even when a `serve --socket` server is running.
    #[arg(long, global = true)]
    no_server: bool,
    /// Say on stderr where the work runs (server or in-process).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Default)]
pub struct Output {
    /// Output format [default: table on a terminal, tsv when piped].
    #[arg(long, value_enum)]
    pub format: Option<Format>,
    /// The server's JSON (dbcore's models) instead.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,
    /// Cut cells wider than this many columns (0: never). On a terminal the default is 64.
    #[arg(long, value_name = "N")]
    pub max_width: Option<usize>,
}

#[derive(Args, Clone)]
pub struct On {
    /// Connection name, id or URL.
    #[arg(value_name = "CONN")]
    pub conn: String,
    /// Database on the connection's server [default: the remembered one, else its own].
    #[arg(short, long)]
    pub database: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// List the saved connections.
    Ls {
        #[command(flatten)]
        out: Output,
    },
    /// List the databases on a connection's server.
    Dbs {
        #[arg(value_name = "CONN")]
        conn: String,
        #[command(flatten)]
        out: Output,
    },
    /// List tables and views.
    Tables {
        #[command(flatten)]
        on: On,
        /// Only this schema.
        #[arg(long)]
        schema: Option<String>,
        /// List again instead of using a server's cache.
        #[arg(long)]
        refresh: bool,
        #[command(flatten)]
        out: Output,
    },
    /// Show a table's columns, indexes and foreign keys.
    Describe {
        #[command(flatten)]
        on: On,
        /// Table or view (`name` or `schema.name`).
        table: String,
        #[arg(long)]
        schema: Option<String>,
        /// Print only its CREATE statement.
        #[arg(long, conflicts_with = "json")]
        ddl: bool,
        #[command(flatten)]
        out: Output,
    },
    /// Show a table's rows (sorted by its key, a page at a time).
    Rows {
        #[command(flatten)]
        on: On,
        table: String,
        #[arg(long)]
        schema: Option<String>,
        /// A SQL condition, e.g. "status = 'paid'".
        #[arg(long, short = 'w')]
        filter: Option<String>,
        /// Sort by a column (`name`, `name:desc` or `-name`); repeat for more.
        #[arg(long, allow_hyphen_values = true)]
        sort: Vec<String>,
        #[arg(long, short = 'n', default_value_t = wire::DEFAULT_PAGE)]
        limit: u32,
        /// The `next` cursor of a previous page (see --json).
        #[arg(long)]
        cursor: Option<String>,
        #[command(flatten)]
        out: Output,
    },
    /// Run SQL (an argument, -f FILE, or stdin). Ctrl-C cancels it.
    Query {
        #[command(flatten)]
        on: On,
        /// The SQL; `-` reads stdin.
        sql: Option<String>,
        /// Read the SQL from a file (`-`: stdin).
        #[arg(short, long, conflicts_with = "sql")]
        file: Option<PathBuf>,
        /// Keep at most this many rows (0: all).
        #[arg(long, default_value_t = dbservice::SCRIPT_ROW_LIMIT)]
        max_rows: u32,
        /// Record the run in the connection's history (off: scripts would push out what you ran in the app).
        #[arg(long)]
        history: bool,
        #[command(flatten)]
        out: Output,
    },
    /// Dump a database to a SQL file (progress on stderr). Ctrl-C cancels it.
    Dump {
        #[command(flatten)]
        on: On,
        /// Output file [default: <database>-<date>.sql]; `.gz` compresses.
        file: Option<PathBuf>,
        #[arg(long, conflicts_with = "data_only")]
        schema_only: bool,
        #[arg(long)]
        data_only: bool,
        /// Gzip (also when FILE ends in .gz).
        #[arg(long)]
        gzip: bool,
        /// INSERT statements instead of COPY (Postgres; the others always use INSERTs).
        #[arg(long)]
        inserts: bool,
        /// DROP … IF EXISTS before each CREATE.
        #[arg(long)]
        drop: bool,
        /// MySQL: CREATE DATABASE IF NOT EXISTS + USE.
        #[arg(long)]
        create_database: bool,
        /// Only these schemas (repeat).
        #[arg(long = "schema", value_name = "SCHEMA", conflicts_with = "tables")]
        schemas: Vec<String>,
        /// Only these tables, `schema.name` (repeat).
        #[arg(long = "table", value_name = "TABLE")]
        tables: Vec<String>,
        /// Print the summary as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Run a SQL file (a dump) against a database. Ctrl-C cancels it.
    Restore {
        #[command(flatten)]
        on: On,
        file: PathBuf,
        /// Don't wrap it in one transaction (the default except on MySQL).
        #[arg(long)]
        no_transaction: bool,
        /// Keep going after a failed statement (without a transaction).
        #[arg(long)]
        continue_on_error: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show what ran on a connection, newest first.
    History {
        #[arg(value_name = "CONN")]
        conn: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        out: Output,
    },
    /// Print a connection's URL.
    Url {
        #[arg(value_name = "CONN")]
        conn: String,
        /// Include the password from the keychain.
        #[arg(long)]
        with_password: bool,
    },
    /// Serve JSON-RPC 2.0 (one JSON message per line) on stdin/stdout, or on a Unix socket.
    Serve {
        /// Listen on a Unix socket [default: serve.sock beside the connection store].
        #[arg(long, value_name = "PATH", num_args = 0..=1)]
        socket: Option<Option<PathBuf>>,
    },
}

/// A failed command: the exit code and what to say on stderr.
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: Option<String>,
}

impl Failure {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self { code, message: Some(message.into()) }
    }

    pub fn quiet(code: u8) -> Self {
        Self { code, message: None }
    }
}

impl From<rpc::RpcError> for Failure {
    fn from(error: rpc::RpcError) -> Self {
        let code = if error.is_cancelled() { CANCELLED } else { FAILED };
        Self::new(code, error.message)
    }
}

impl From<dbcore::Error> for Failure {
    fn from(error: dbcore::Error) -> Self {
        let code = if error == dbcore::Error::Cancelled { CANCELLED } else { FAILED };
        Self::new(code, error.to_string())
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        // `dbear-cli … | head`: the reader left, not an error.
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            return Self::quiet(0);
        }
        Self::new(FAILED, error.to_string())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("dbear-cli: {e}");
            return ExitCode::from(FAILED);
        }
    };
    let code = match runtime.block_on(commands::run(cli.command, cli.no_server, cli.verbose)) {
        Ok(()) => 0,
        Err(failure) => {
            if let Some(message) = failure.message {
                eprintln!("dbear-cli: {message}");
            }
            failure.code
        }
    };
    // Don't wait for connections still closing on other threads.
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    ExitCode::from(code)
}
