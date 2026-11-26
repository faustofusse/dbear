//! dbear-cli's tests, in one binary: they spawn the built `dbear-cli` against a temp connection
//! store holding a SQLite file and a mock (sample) connection. Nothing touches the user's store,
//! state, keychain or a running server: `DBEAR_CONNECTIONS_FILE`, `DBEAR_SOCKET`, `HOME` and
//! `DBEAR_NO_KEYRING=1` point everything at the temp dir.

mod edit;
mod oneshot;
mod serve;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use dbcore::{ConnectionConfig, ConnectionStore, DatabaseKind};
use serde_json::{json, Value as Json};

pub const BIN: &str = env!("CARGO_BIN_EXE_dbear-cli");

pub struct Fixture {
    pub dir: tempfile::TempDir,
    /// The SQLite connection's id ("App", file app.db).
    pub app: String,
}

#[allow(clippy::new_without_default)]
impl Fixture {
    /// A store with "App" (SQLite, seeded) and "Staging" (a mock Postgres served by dbcore's sample driver).
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(dir.path().join("dbear.db")).unwrap();
        for file in ["app.db", "copy.db", "big.db"] {
            std::fs::write(dir.path().join(file), b"").unwrap();
        }
        let mut app = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
        app.name = "App".into();
        app.database = dir.path().join("app.db").display().to_string();
        let app = store.upsert(app).unwrap();
        let staging = dbcore::mock::connections().into_iter().find(|c| c.id == "staging-pg").unwrap();
        store.upsert(ConnectionConfig { name: "Staging".into(), ..staging }).unwrap();
        drop(store);
        let fx = Self { dir, app: app.id };
        fx.ok(&[
            "query",
            "App",
            "create table users (id integer primary key, name text, score real, note text);\
             insert into users values (1, 'Ana', 1.5, null), (2, '日本語', 20, 'two\nlines'), (3, 'Bo', null, 'NULL');\
             create view named as select id, name from users;",
        ]);
        fx
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn socket(&self) -> PathBuf {
        self.path("serve.sock")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("DBEAR_CONNECTIONS_FILE", self.path("dbear.db"))
            .env("DBEAR_SOCKET", self.socket())
            .env("DBEAR_NO_KEYRING", "1")
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path())
            .env("NO_COLOR", "1")
            .env_remove("DBEAR_UPDATE_FEED");
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, None)
    }

    pub fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = self.command(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(stdin) = stdin {
            input.write_all(stdin.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }

    /// Runs and expects success; stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?} failed ({:?}): {}", out.status, String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs and expects exit `code`; stderr.
    pub fn fails(&self, args: &[&str], code: i32) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stderr).unwrap()
    }
}

/// A JSON-RPC peer over a child's stdio or a socket: a thread reads lines into a channel.
pub struct Peer {
    write: Box<dyn Write + Send>,
    lines: mpsc::Receiver<Json>,
    /// What arrived while waiting for something else.
    seen: Vec<Json>,
    pub child: Option<Child>,
}

impl Peer {
    pub fn new(read: impl std::io::Read + Send + 'static, write: impl Write + Send + 'static) -> Self {
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines() {
                let Ok(line) = line else { break };
                let json: Json = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                if tx.send(json).is_err() {
                    break;
                }
            }
        });
        Self { write: Box::new(write), lines, seen: Vec::new(), child: None }
    }

    /// `dbear-cli serve` on stdio.
    pub fn stdio(fx: &Fixture) -> Self {
        let mut child = fx.command(&["serve"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let (read, write) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let mut peer = Self::new(read, write);
        peer.child = Some(child);
        peer
    }

    #[cfg(unix)]
    pub fn socket(path: &Path) -> Self {
        let stream = std::os::unix::net::UnixStream::connect(path).unwrap();
        Self::new(stream.try_clone().unwrap(), stream)
    }

    pub fn send_raw(&mut self, line: &str) {
        self.write.write_all(format!("{line}\n").as_bytes()).unwrap();
        self.write.flush().unwrap();
    }

    pub fn send(&mut self, id: impl Into<Json>, method: &str, params: Json) {
        let line = json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params}).to_string();
        self.send_raw(&line);
    }

    /// The next message matching `wanted` (others are kept for later).
    pub fn wait(&mut self, wanted: impl Fn(&Json) -> bool) -> Json {
        if let Some(i) = self.seen.iter().position(&wanted) {
            return self.seen.remove(i);
        }
        loop {
            let message = self.lines.recv_timeout(Duration::from_secs(20)).unwrap_or_else(|e| panic!("no answer ({e}); seen {:?}", self.seen));
            if wanted(&message) {
                return message;
            }
            self.seen.push(message);
        }
    }

    pub fn response(&mut self, id: impl Into<Json>) -> Json {
        let id = id.into();
        self.wait(|m| m.get("id") == Some(&id) && m.get("method").is_none())
    }

    /// Sends and returns the `result` (panics on an error).
    pub fn call(&mut self, id: impl Into<Json> + Clone, method: &str, params: Json) -> Json {
        self.send(id.clone(), method, params);
        let response = self.response(id);
        assert!(response.get("error").is_none(), "{method}: {response}");
        assert_eq!(response["jsonrpc"], "2.0");
        response["result"].clone()
    }

    /// Sends and returns the `error`.
    pub fn error(&mut self, id: impl Into<Json> + Clone, method: &str, params: Json) -> Json {
        self.send(id.clone(), method, params);
        let response = self.response(id);
        assert!(response.get("result").is_none(), "{method}: {response}");
        response["error"].clone()
    }

    pub fn notification(&mut self, method: &str, job: &str) -> Json {
        self.wait(|m| m["method"] == method && m["params"]["job"] == job)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A query that runs until it's cancelled (SQLite).
pub const ENDLESS: &str = "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c";

/// Waits for `check` (polling), up to 10 s.
pub fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Whether `a` names `b` (temp dirs may be reached through a symlink, e.g. /tmp → /private/tmp).
pub fn same_file(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b || std::fs::canonicalize(a.parent().unwrap()).ok() == std::fs::canonicalize(b.parent().unwrap()).ok() && a.file_name() == b.file_name()
}
