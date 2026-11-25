//! One-shot commands, in-process (no server is running for these fixtures).

use std::process::Stdio;

use serde_json::Value as Json;

use crate::{Fixture, ENDLESS};

#[test]
fn ls_lists_saved_connections_without_secrets() {
    let fx = Fixture::new();
    let tsv = fx.ok(&["ls"]);
    let lines: Vec<&str> = tsv.lines().collect();
    assert_eq!(lines[0], "name\tkind\tgroup\tsummary\tid");
    assert!(lines[1].starts_with(&format!("App\tSQLite\t\tSQLite · {}", fx.path("app.db").display())), "{tsv}");
    assert!(lines[2].starts_with("Staging\tPostgreSQL\tStaging\tPostgreSQL · staging-db.internal:5432/app\tstaging-pg"), "{tsv}");
    let json: Json = serde_json::from_str(&fx.ok(&["ls", "--json"])).unwrap();
    let list = json["connections"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], fx.app.as_str());
    assert_eq!(list[1]["kind"], "postgres");
    assert_eq!(list[1]["browses_databases"], true);
    assert!(list.iter().all(|c| c.get("password").is_none()));
    let table = fx.ok(&["ls", "--format", "table"]);
    assert!(table.starts_with(" name    │ kind       │ group   │ summary"), "{table}");
}

#[test]
fn query_formats() {
    let fx = Fixture::new();
    let sql = "select id, name, score, note from users order by id";
    let tsv = fx.ok(&["query", "App", sql]);
    assert_eq!(tsv, "id\tname\tscore\tnote\n1\tAna\t1.5\t\n2\t日本語\t20\t\"two\nlines\"\n3\tBo\t\tNULL\n");
    let table = fx.ok(&["query", "App", sql, "--format", "table"]);
    // Display widths (日本語 is 6 columns), numbers right-aligned, NULL apart from the text "NULL",
    // line breaks escaped, a footer.
    let expected = " id │ name   │  score │ note\n\
                    ────┼────────┼────────┼────────────\n  \
                    1 │ Ana    │    1.5 │ <null>\n  \
                    2 │ 日本語 │     20 │ two\\nlines\n  \
                    3 │ Bo     │ <null> │ NULL\n";
    assert!(table.starts_with(expected), "{table}");
    assert!(table.ends_with(" ms)\n") && table.contains("(3 rows, "), "{table}");
    let csv = fx.ok(&["query", "App", sql, "--format", "csv"]);
    assert_eq!(csv, "id,name,score,note\r\n1,Ana,1.5,\r\n2,日本語,20,\"two\nlines\"\r\n3,Bo,,NULL\r\n");
    let json: Json = serde_json::from_str(&fx.ok(&["query", "App", sql, "--format", "json"])).unwrap();
    assert_eq!(json[0], serde_json::json!({"id": 1, "name": "Ana", "score": 1.5, "note": null}));
    assert_eq!(json[2]["note"], "NULL");
    let markdown = fx.ok(&["query", "App", "select id, name from users where id = 1", "--format", "markdown"]);
    assert_eq!(markdown, "| id | name |\n| --- | --- |\n| 1 | Ana |\n");
    let insert = fx.ok(&["query", "App", "select id, name from users where id = 1", "--format", "insert"]);
    assert_eq!(insert, "insert into \"table_name\" (\"id\", \"name\") values (1, 'Ana');\n");
    // --json: the wire model (lossless values).
    let wire: Json = serde_json::from_str(&fx.ok(&["query", "App", "select 9007199254740993 as big, 0.5 as f", "--json"])).unwrap();
    assert_eq!(wire["result"]["ok"]["rows"][0], serde_json::json!([{"int": "9007199254740993"}, {"float": 0.5}]));
    assert_eq!(wire["target"]["connection_id"], fx.app.as_str());
}

#[test]
fn wide_cells_are_cut_only_when_asked_or_on_a_terminal() {
    let fx = Fixture::new();
    let sql = "select 'abcdefghijklmnopqrstuvwxyz' as letters";
    // Piped: never cut, even as a table.
    assert!(fx.ok(&["query", "App", sql, "--format", "table"]).contains("abcdefghijklmnopqrstuvwxyz"));
    let cut = fx.ok(&["query", "App", sql, "--format", "table", "--max-width", "10"]);
    assert!(cut.contains(" abcdefghi…\n"), "{cut}");
}

#[test]
fn sql_from_argument_stdin_and_files() {
    let fx = Fixture::new();
    let expected = "n\n3\n";
    let sql = "select count(*) as n from users";
    assert_eq!(fx.ok(&["query", "App", sql]), expected);
    let piped = fx.run_with_stdin(&["query", "App"], Some(sql));
    assert_eq!(String::from_utf8(piped.stdout).unwrap(), expected);
    let dash = fx.run_with_stdin(&["query", "App", "-"], Some(sql));
    assert_eq!(String::from_utf8(dash.stdout).unwrap(), expected);
    std::fs::write(fx.path("q.sql"), sql).unwrap();
    assert_eq!(fx.ok(&["query", "App", "-f", "q.sql"]), expected);
    let file_dash = fx.run_with_stdin(&["query", "App", "-f", "-"], Some(sql));
    assert_eq!(String::from_utf8(file_dash.stdout).unwrap(), expected);
    // Statements without rows say so on stderr when piped.
    let out = fx.run(&["query", "App", "update users set score = score where id < 3"]);
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("(2 rows affected, "));
    let table = fx.ok(&["query", "App", "update users set score = score", "--format", "table"]);
    assert!(table.starts_with("(3 rows affected, "), "{table}");
}

#[test]
fn errors_and_exit_codes() {
    let fx = Fixture::new();
    let stderr = fx.fails(&["query", "App", "selec 1"], 1);
    assert!(stderr.starts_with("dbear-cli: ") && stderr.contains("syntax error"), "{stderr}");
    let stderr = fx.fails(&["query", "Nope", "select 1"], 1);
    assert!(stderr.contains("No saved connection is called “Nope”"), "{stderr}");
    assert!(fx.fails(&["query", "App", "-f", "missing.sql"], 1).contains("missing.sql"));
    let empty = fx.run_with_stdin(&["query", "App"], Some("  \n"));
    assert_eq!(empty.status.code(), Some(2));
    fx.fails(&["query"], 2);
    fx.fails(&["frobnicate"], 2);
    fx.fails(&["query", "App", "select 1", "--format", "yaml"], 2);
    assert!(fx.fails(&["describe", "App", "nope"], 1).contains("Table not found: nope"));
    assert!(fx.fails(&["tables", "App", "--schema", "nope"], 1).contains("no schema"));
    assert!(fx.fails(&["dump", "App", "x.sql", "--table", "users"], 2).contains("schema.name"));
    // --json still fails with the query's code, after printing the outcome.
    let out = fx.run(&["query", "App", "selec 1", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let wire: Json = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(wire["result"]["error"]["kind"], "query");
}

#[test]
fn row_cap() {
    let fx = Fixture::new();
    let out = fx.run(&["query", "App", "select id from users order by id", "--max-rows", "2"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "id\n1\n2\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("showing the first 2 of 3 rows"));
    assert_eq!(fx.ok(&["query", "App", "select id from users", "--max-rows", "0"]).lines().count(), 4);
}

#[test]
fn metadata_commands() {
    let fx = Fixture::new();
    assert_eq!(fx.ok(&["tables", "App"]), "schema\tname\tkind\trows\nmain\tnamed\tview\t\nmain\tusers\ttable\t3\n");
    assert_eq!(fx.ok(&["dbs", "App"]), format!("database\n{}\n", fx.path("app.db").display()));
    let described = fx.ok(&["describe", "App", "users", "--format", "table"]);
    assert!(described.starts_with("main.users (table)\n\n column │ type    │ nullable │ default │ key │ comment\n"), "{described}");
    assert!(described.contains(" id     │ integer │ no       │         │ PK  │"), "{described}");
    let ddl = fx.ok(&["describe", "App", "main.users", "--ddl"]);
    assert!(ddl.to_lowercase().starts_with("create table users"), "{ddl}");
    let json: Json = serde_json::from_str(&fx.ok(&["describe", "App", "users", "--json"])).unwrap();
    assert_eq!(json["table"]["schema"], "main");
    assert_eq!(json["structure"]["primary_key"], serde_json::json!(["id"]));
    let rows = fx.ok(&["rows", "App", "users", "--sort", "-id", "--limit", "2", "--format", "csv"]);
    assert_eq!(rows, "id,name,score,note\r\n3,Bo,,NULL\r\n2,日本語,20,\"two\nlines\"\r\n");
    let page: Json = serde_json::from_str(&fx.ok(&["rows", "App", "users", "--limit", "2", "--json"])).unwrap();
    let cursor = page["next"].as_str().unwrap();
    let rest = fx.ok(&["rows", "App", "users", "--limit", "2", "--cursor", cursor, "--format", "tsv"]);
    assert_eq!(rest, "id\tname\tscore\tnote\n3\tBo\t\tNULL\n");
    let filtered = fx.ok(&["rows", "App", "users", "-w", "score > 10", "--format", "tsv"]);
    assert!(filtered.contains("日本語") && !filtered.contains("Ana") && !filtered.contains("Bo"), "{filtered}");
    assert_eq!(fx.ok(&["url", "App"]), format!("sqlite://{}\n", fx.path("app.db").display()));
}

#[test]
fn mock_connection() {
    let fx = Fixture::new();
    let tables = fx.ok(&["tables", "Staging", "--schema", "billing"]);
    assert!(tables.starts_with("schema\tname\tkind\trows\nbilling\tinvoices\ttable\t930\n"), "{tables}");
    assert_eq!(fx.ok(&["dbs", "staging-pg"]).lines().next(), Some("database"));
    let users = fx.ok(&["query", "staging", "select id, name from users limit 3"]);
    assert_eq!(users.lines().count(), 4, "{users}");
    let structure: Json = serde_json::from_str(&fx.ok(&["describe", "Staging", "invoices", "--json"])).unwrap();
    assert_eq!(structure["table"]["schema"], "billing");
    assert!(fx.fails(&["query", "Staging", "delete from users"], 1).contains("mock driver"));
    assert!(fx.ok(&["url", "Staging"]).starts_with("postgres://readonly@staging-db.internal:5432/app"));
}

#[test]
fn history_only_when_asked() {
    let fx = Fixture::new();
    fx.ok(&["query", "App", "select 1 as one"]);
    assert_eq!(fx.ok(&["history", "App"]), "ran_at\tdatabase\tms\trows\terror\tsql\n");
    fx.ok(&["query", "App", "select   2 as two", "--history"]);
    fx.fails(&["query", "App", "selec 3", "--history"], 1);
    let history = fx.ok(&["history", "App"]);
    let lines: Vec<&str> = history.lines().collect();
    assert_eq!(lines.len(), 3, "{history}");
    assert!(lines[1].ends_with("selec 3") && lines[1].contains("syntax error"), "{history}");
    assert!(lines[2].ends_with("\t1\t\tselect 2 as two"), "{history}");
    let json: Json = serde_json::from_str(&fx.ok(&["history", "App", "-n", "1", "--json"])).unwrap();
    assert_eq!(json["entries"].as_array().unwrap().len(), 1);
    assert_eq!(json["entries"][0]["sql"], "selec 3");
}

#[test]
fn dump_and_restore() {
    let fx = Fixture::new();
    let out = fx.run(&["dump", "App", "app.sql.gz"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Dumping") && stderr.contains("Dumped 1 table, 3 rows to "), "{stderr}");
    let gz = std::fs::read(fx.path("app.sql.gz")).unwrap();
    assert_eq!(&gz[..2], &[0x1f, 0x8b], "gzipped because of the name");
    let summary: Json = serde_json::from_str(&fx.ok(&["dump", "App", "plain.sql", "--json", "--table", "main.users"])).unwrap();
    assert_eq!(summary["summary"]["rows"], 3);
    assert!(summary["path"].as_str().unwrap().ends_with("plain.sql"));
    assert!(std::fs::read_to_string(fx.path("plain.sql")).unwrap().to_lowercase().contains("insert into"));
    // Into a new SQLite file through a URL connection (not saved).
    let copy = format!("sqlite:{}", fx.path("copy.db").display());
    let out = fx.run(&["restore", &copy, "app.sql.gz"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Restored "));
    assert_eq!(fx.ok(&["query", &copy, "select count(*) as n from users"]), "n\n3\n");
    // The tables exist already: it stops at the first failure (one transaction, rolled back) and exits 1.
    let failed = fx.run(&["restore", &copy, "plain.sql"]);
    assert_eq!(failed.status.code(), Some(1), "{}", String::from_utf8_lossy(&failed.stderr));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("already exists"));
    // Going on after errors: each is listed, the summary still printed, exit 1.
    let out = fx.run(&["restore", &copy, "plain.sql", "--json", "--continue-on-error", "--no-transaction"]);
    assert_eq!(out.status.code(), Some(1));
    let json: Json = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["summary"]["error_count"].as_u64().unwrap() >= 2, "{json}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("UNIQUE constraint failed"));
    assert!(fx.fails(&["restore", "App", "nope.sql"], 1).contains("isn’t a file"));
    // No file name: <database>-<date>.sql in the working directory.
    fx.ok(&["dump", "App", "--schema-only"]);
    let named = std::fs::read_dir(fx.dir.path()).unwrap().filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        name.starts_with("app-") && name.ends_with(".sql")
    });
    assert!(named);
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_the_query() {
    let fx = Fixture::new();
    let child = fx.command(&["query", "App", ENDLESS]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let killed = std::process::Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cancelling") && stderr.contains("Query cancelled"), "{stderr}");
}
