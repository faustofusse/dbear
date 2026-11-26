//! `serve` over stdio and over a socket.

use serde_json::{json, Value as Json};

use crate::{eventually, Fixture, Peer, ENDLESS};

#[test]
fn stdio_methods() {
    let fx = Fixture::new();
    let mut peer = Peer::stdio(&fx);

    let info = peer.call(1, "server.info", json!({}));
    assert_eq!(info["name"], "dbear-cli");
    assert_eq!(info["protocol"], 1);
    assert_eq!(info["transport"], "stdio");
    assert_eq!(info["problems"], json!([]));
    assert!(info["store_path"].as_str().unwrap().ends_with("dbear.db"));

    let list = peer.call(2, "connections.list", Json::Null);
    let names: Vec<&str> = list["connections"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["App", "Staging"]);
    let got = peer.call("get", "connections.get", json!({"connection": "staging"}));
    assert_eq!(got["id"], "staging-pg");
    assert_eq!(got["target"], json!({"connection_id": "staging-pg", "database": "app"}));
    let url = peer.call(3, "connections.url", json!({"connection": "Staging"}));
    assert_eq!(url["url"], "postgres://readonly@staging-db.internal:5432/app");

    let dbs = peer.call(4, "databases.list", json!({"connection": "Staging"}));
    assert_eq!(dbs["default"], "app");
    assert!(dbs["databases"].as_array().unwrap().contains(&json!("app")));

    let schemas = peer.call(5, "schemas.list", json!({"connection": "App"}));
    assert_eq!(schemas["target"]["connection_id"], fx.app.as_str());
    assert_eq!(schemas["schemas"][0]["name"], "main");
    assert_eq!(schemas["schemas"][0]["tables"][1], json!({"schema": "main", "name": "users", "kind": "table", "estimated_row_count": 3}));

    let described = peer.call(6, "table.describe", json!({"connection": "App", "table": "users"}));
    assert_eq!(described["structure"]["columns"][1]["name"], "name");

    let rows = peer.call(7, "table.rows", json!({"connection": "App", "table": "users", "limit": 2, "query": {"sort": [{"column": "id", "descending": true}]}}));
    assert_eq!(rows["result"]["rows"], json!([[3, "Bo", null, "NULL"], [2, "日本語", {"float": 20.0}, "two\nlines"]]));
    let next = rows["next"].as_str().unwrap().to_string();
    let rest = peer.call(8, "table.rows", json!({"connection": "App", "table": "users", "limit": 2, "cursor": next, "query": {"sort": [{"column": "id", "descending": true}]}}));
    assert_eq!(rest["result"]["rows"], json!([[1, "Ana", {"float": 1.5}, null]]));
    assert_eq!(rest["next"], Json::Null);

    let run = peer.call(9, "query.run", json!({"connection": "App", "sql": "select name from users order by id", "max_rows": 2}));
    assert_eq!(run["result"]["ok"]["rows"], json!([["Ana"], ["日本語"]]));
    assert_eq!(run["result"]["ok"]["truncated"], true);
    assert!(run["duration_ms"].is_u64());
    let failed = peer.call(10, "query.run", json!({"connection": "App", "sql": "selec", "record_history": true}));
    assert_eq!(failed["result"]["error"]["kind"], "query");
    let ddl = peer.call(11, "query.run", json!({"connection": "App", "sql": "create table extra (x int)", "record_history": true}));
    assert_eq!(ddl["schema_changed"], true);
    let schemas = peer.call(12, "schemas.list", json!({"connection": "App"}));
    assert!(schemas["schemas"][0]["tables"].as_array().unwrap().iter().any(|t| t["name"] == "extra"));
    let sources = peer.call(13, "query.run", json!({"connection": "App", "sql": "select id, name from users", "describe_sources": true}));
    assert!(sources["sources"].is_object(), "{sources}");

    let completions = peer.call(14, "complete", json!({"connection": "App", "sql": "select * from us", "offset": 16}));
    assert_eq!(completions["replace_start"], 14);
    assert!(completions["items"].as_array().unwrap().iter().any(|i| i["label"] == "users"));
    let columns = peer.call(15, "complete", json!({"connection": "App", "sql": "select sco from users", "offset": 10}));
    assert!(columns["items"].as_array().unwrap().iter().any(|i| i["label"] == "score"), "{columns}");

    let history = peer.call(16, "history.list", json!({"connection": "App"}));
    let sql: Vec<&str> = history["entries"].as_array().unwrap().iter().map(|e| e["sql"].as_str().unwrap()).collect();
    assert_eq!(sql, ["create table extra (x int)", "selec"]);

    let disconnected = peer.call(17, "disconnect", json!({"connection": "App"}));
    assert_eq!(disconnected["connections"], json!([fx.app]));
    // The rest: the shared metadata session `databases.list` opened on Staging.
    assert_eq!(peer.call(18, "disconnect", json!({}))["connections"], json!(["staging-pg"]));
    // Still works after: connections reopen.
    assert_eq!(peer.call(19, "query.run", json!({"connection": "App", "sql": "select 1 as x"}))["result"]["ok"]["rows"], json!([[1]]));

    // A notification (no id) gets no answer; the next request does.
    peer.send_raw(r#"{"jsonrpc":"2.0","method":"server.info"}"#);
    peer.call(20, "server.info", json!({}));
    assert!(peer.seen.is_empty(), "{:?}", peer.seen);

    peer.call(21, "server.shutdown", json!({}));
    let status = peer.child.as_mut().unwrap().wait().unwrap();
    assert!(status.success());
}

#[test]
fn columns_list() {
    let fx = Fixture::new();
    let mut peer = Peer::stdio(&fx);
    let got = peer.call(1, "columns.list", json!({"connection": "App"}));
    assert_eq!(got["target"]["connection_id"], fx.app.as_str());
    let tables = got["tables"].as_array().unwrap();
    let users = tables.iter().find(|t| t["table"] == "users").unwrap();
    assert_eq!(users["schema"], "main");
    assert_eq!(users["columns"][0], json!({"name": "id", "type_name": "integer", "is_primary_key": true, "is_nullable": false}));
    assert_eq!(users["columns"].as_array().unwrap().len(), 4);
    // Views too.
    let named = tables.iter().find(|t| t["table"] == "named").unwrap();
    let names: Vec<&str> = named["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["id", "name"]);

    // Another database on a server (the mock Postgres).
    let other = peer.call(2, "columns.list", json!({"connection": "Staging", "database": "app_test"}));
    assert_eq!(other["target"], json!({"connection_id": "staging-pg", "database": "app_test"}));
    assert!(!other["tables"].as_array().unwrap().is_empty());

    let err = peer.error(3, "columns.list", json!({"connection": "ghost"}));
    assert_eq!(err["data"]["kind"], "invalid-config");
}

#[test]
fn stdio_errors() {
    let fx = Fixture::new();
    let mut peer = Peer::stdio(&fx);
    peer.send_raw("this isn't json");
    let parse = peer.wait(|m| m["error"]["code"] == -32700);
    assert_eq!(parse["id"], Json::Null);
    peer.send_raw(r#"{"jsonrpc":"2.0","id":1}"#);
    assert_eq!(peer.response(1)["error"]["code"], -32600);
    let unknown = peer.error(2, "nope", json!({}));
    assert_eq!(unknown["code"], -32601);
    let invalid = peer.error(3, "query.run", json!({"connection": "App"}));
    assert_eq!(invalid["code"], -32602);
    assert_eq!(invalid["data"]["kind"], "invalid-config");
    let missing = peer.error(4, "schemas.list", json!({"connection": "Nope"}));
    assert_eq!(missing["code"], -32000);
    assert_eq!(missing["data"]["kind"], "invalid-config");
    assert!(missing["message"].as_str().unwrap().contains("Nope"));
    let table = peer.error("t", "table.describe", json!({"connection": "App", "table": "ghost"}));
    assert_eq!(table["data"], json!({"kind": "table-not-found", "message": "ghost"}));
    assert_eq!(table["message"], "Table not found: ghost");
    let cursor = peer.error(5, "table.rows", json!({"connection": "App", "table": "users", "cursor": "garbage"}));
    assert_eq!(cursor["code"], -32000);
    // Still serving.
    peer.call(6, "server.info", json!({}));
}

#[test]
fn concurrent_query_and_cancel() {
    let fx = Fixture::new();
    let mut peer = Peer::stdio(&fx);
    peer.send("slow", "query.run", json!({"connection": "App", "sql": ENDLESS}));
    // Other requests of the same client are answered meanwhile.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(peer.call(1, "schemas.list", json!({"connection": "App"}))["schemas"][0]["name"], "main");
    assert_eq!(peer.call(2, "query.run", json!({"connection": "Staging", "sql": "select id from users limit 1"}))["result"]["ok"]["rows"], json!([[1]]));
    assert_eq!(peer.call(3, "query.cancel", json!({"id": "other"}))["cancelled"], 0);
    assert_eq!(peer.call(4, "query.cancel", json!({"id": "slow"}))["cancelled"], 1);
    let slow = peer.response("slow");
    assert_eq!(slow["result"]["result"], json!({"error": {"kind": "cancelled"}}));
    // By target.
    peer.send(5, "query.run", json!({"connection": "App", "sql": ENDLESS}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(peer.call(6, "query.cancel", json!({"connection": "App"}))["cancelled"], 1);
    assert_eq!(peer.response(5)["result"]["result"]["error"]["kind"], "cancelled");
    // Cancelled runs aren't in the history even when asked.
    peer.send(7, "query.run", json!({"connection": "App", "sql": ENDLESS, "record_history": true}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    peer.call(8, "query.cancel", json!({}));
    peer.response(7);
    assert_eq!(peer.call(9, "history.list", json!({"connection": "App"}))["entries"], json!([]));
}

#[test]
fn dump_and_restore_jobs() {
    let fx = Fixture::new();
    let mut peer = Peer::stdio(&fx);
    let started = peer.call(1, "dump.start", json!({"connection": "App", "path": "out.sql"}));
    assert_eq!(started["job"], "dump-1");
    assert!(crate::same_file(started["path"].as_str().unwrap(), &fx.path("out.sql")), "{started}");
    let progress = peer.notification("dump.progress", "dump-1");
    assert!(progress["params"]["progress"]["phase"].is_string());
    assert_eq!(progress["jsonrpc"], "2.0");
    assert!(progress.get("id").is_none());
    let finished = peer.notification("dump.finished", "dump-1");
    assert_eq!(finished["params"]["result"]["ok"]["rows"], 3);
    assert!(fx.path("out.sql").is_file());

    let copy = format!("sqlite:{}", fx.path("copy.db").display());
    let started = peer.call(2, "restore.start", json!({"connection": copy, "path": fx.path("out.sql")}));
    let job = started["job"].as_str().unwrap().to_string();
    let finished = peer.notification("restore.finished", &job);
    assert!(finished["params"]["result"]["ok"]["statements"].as_u64().unwrap() > 0, "{finished}");
    assert_eq!(peer.call(3, "query.run", json!({"connection": copy, "sql": "select count(*) from users"}))["result"]["ok"]["rows"], json!([[3]]));
    let progress = peer.notification("restore.progress", &job);
    assert!(progress["params"]["progress"]["bytes_total"].as_u64().unwrap() > 0);

    // Cancelling a job: it finishes with a cancelled error and leaves no file behind.
    let big = format!("sqlite:{}", fx.path("big.db").display());
    let rows = peer.call(4, "query.run", json!({"connection": big, "sql": "create table t (x text); with recursive c(x) as (select 1 union all select x + 1 from c where x < 300000) insert into t select printf('%0100d', x) from c"}));
    assert!(rows["result"]["ok"].is_object(), "{rows}");
    let started = peer.call(5, "dump.start", json!({"connection": big, "path": "big.sql"}));
    let job = started["job"].as_str().unwrap().to_string();
    assert_eq!(peer.call(6, "job.cancel", json!({"job": job}))["cancelled"], true);
    let finished = peer.notification("dump.finished", &job);
    assert_eq!(finished["params"]["result"], json!({"error": {"kind": "cancelled"}}));
    assert!(!fx.path("big.sql").exists());
    assert_eq!(peer.call(7, "job.cancel", json!({"job": job}))["cancelled"], false);
    let error = peer.error(8, "restore.start", json!({"connection": "App", "path": "missing.sql"}));
    assert_eq!(error["data"]["kind"], "invalid-config");
}

#[test]
fn stdio_answers_in_flight_requests_after_eof() {
    let fx = Fixture::new();
    let mut child = fx.command(&["serve"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"query.run","params":{{"connection":"App","sql":"select count(*) from users"}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"dump.start","params":{{"connection":"App","path":"eof.sql"}}}}"#).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines: Vec<Json> = String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.iter().any(|l| l["id"] == 1 && l["result"]["result"]["ok"]["rows"] == json!([[3]])));
    assert!(lines.iter().any(|l| l["method"] == "dump.finished"));
    assert!(fx.path("eof.sql").is_file());
}

#[cfg(unix)]
mod socket {
    use std::process::{Child, Stdio};

    use super::*;

    struct Server(Child);

    impl Server {
        fn start(fx: &Fixture, args: &[&str]) -> Self {
            let child = fx.command(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let socket = fx.socket();
            eventually("the socket", || std::os::unix::net::UnixStream::connect(&socket).is_ok());
            Self(child)
        }

        fn terminate(mut self) -> std::process::ExitStatus {
            std::process::Command::new("kill").args(["-TERM", &self.0.id().to_string()]).status().unwrap();
            self.0.wait().unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn two_clients_at_once() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let server = Server::start(&fx, &["serve", "--socket"]);
        let mode = std::fs::metadata(fx.socket()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mut a = Peer::socket(&fx.socket());
        let mut b = Peer::socket(&fx.socket());
        assert_eq!(a.call(1, "server.info", json!({}))["transport"], "socket");
        // Temp tables live in a client's own session.
        a.call(2, "query.run", json!({"connection": "App", "sql": "create temp table mine (x int); insert into mine values (1)"}));
        assert_eq!(a.call(3, "query.run", json!({"connection": "App", "sql": "select x from mine"}))["result"]["ok"]["rows"], json!([[1]]));
        let theirs = b.call(3, "query.run", json!({"connection": "App", "sql": "select x from mine"}));
        assert_eq!(theirs["result"]["error"]["kind"], "query", "{theirs}");
        // A long query of one client, the other carries on, and cancelling is per client.
        a.send("slow", "query.run", json!({"connection": "App", "sql": ENDLESS}));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(b.call(4, "query.cancel", json!({}))["cancelled"], 0);
        assert_eq!(b.call(5, "query.run", json!({"connection": "App", "sql": "select count(*) from users"}))["result"]["ok"]["rows"], json!([[3]]));
        assert_eq!(a.call(6, "query.cancel", json!({"id": "slow"}))["cancelled"], 1);
        assert_eq!(a.response("slow")["result"]["result"]["error"]["kind"], "cancelled");
        // A client that hangs up mid-query: its query is cancelled, the server stays up.
        a.send("gone", "query.run", json!({"connection": "App", "sql": ENDLESS}));
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(a);
        assert_eq!(b.call(7, "server.info", json!({}))["protocol"], 1);
        let status = server.terminate();
        assert!(status.success(), "{status:?}");
        assert!(!fx.socket().exists(), "the socket is removed on exit");
    }

    #[test]
    fn one_shot_commands_use_the_running_server() {
        let fx = Fixture::new();
        let server = Server::start(&fx, &["serve", "--socket"]);
        let out = fx.run(&["-v", "query", "App", "select count(*) as n from users"]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "n\n3\n");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(&format!("using the server on {}", fx.socket().display())), "{stderr}");
        let out = fx.run(&["-v", "--no-server", "tables", "App"]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("working in-process"));
        // Every kind of command goes through it the same way.
        assert_eq!(fx.ok(&["tables", "App"]), fx.ok(&["--no-server", "tables", "App"]));
        assert_eq!(fx.ok(&["describe", "App", "users", "--json"]), fx.ok(&["--no-server", "describe", "App", "users", "--json"]));
        fx.ok(&["dump", "App", "via-server.sql"]);
        assert!(fx.path("via-server.sql").is_file());
        // A connection saved after the server started is found (the store is read again).
        let mut store = dbcore::ConnectionStore::open(fx.path("dbear.db")).unwrap();
        let mut later = dbcore::ConnectionConfig::new_empty(dbcore::DatabaseKind::Sqlite);
        later.name = "Later".into();
        later.database = fx.path("app.db").display().to_string();
        store.upsert(later).unwrap();
        assert_eq!(fx.ok(&["query", "Later", "select 1 as x"]), "x\n1\n");
        // Ctrl-C through the server.
        let child = fx.command(&["query", "App", ENDLESS]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(800));
        std::process::Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(130), "{}", String::from_utf8_lossy(&out.stderr));
        // A second server on the same socket refuses to start.
        let second = fx.run(&["serve", "--socket"]);
        assert_eq!(second.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&second.stderr).contains("already listening"));
        assert!(server.terminate().success());
    }

    #[test]
    fn stale_socket_is_replaced_and_ignored_by_clients() {
        let fx = Fixture::new();
        // A socket file nobody listens on (a crashed server).
        drop(std::os::unix::net::UnixListener::bind(fx.socket()).unwrap());
        assert!(fx.socket().exists());
        let out = fx.run(&["-v", "query", "App", "select 1 as x"]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "x\n1\n");
        assert!(String::from_utf8_lossy(&out.stderr).contains("working in-process"));
        let server = Server::start(&fx, &["serve", "--socket"]);
        assert_eq!(Peer::socket(&fx.socket()).call(1, "server.info", json!({}))["transport"], "socket");
        assert!(server.terminate().success());
        assert!(!fx.socket().exists());
    }

    #[test]
    fn explicit_socket_path_and_shutdown() {
        let fx = Fixture::new();
        let path = fx.path("elsewhere.sock");
        let mut child = fx.command(&["serve", "--socket", path.to_str().unwrap()]).stderr(Stdio::null()).spawn().unwrap();
        eventually("the socket", || std::os::unix::net::UnixStream::connect(&path).is_ok());
        let mut peer = Peer::socket(&path);
        peer.call(1, "server.shutdown", json!({}));
        assert!(child.wait().unwrap().success());
        assert!(!path.exists());
    }
}
