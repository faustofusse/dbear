//! `table.editable`, `table.changes.preview` / `table.changes.apply` and `table.related`: row edits
//! sent as data, the SQL built and run by the core.

use serde_json::{json, Value as Json};

use crate::{Fixture, Peer};

fn setup(fx: &Fixture) -> Peer {
    fx.ok(&[
        "query",
        "App",
        "create table orders (id integer primary key, user_id integer references users(id), total numeric not null default 0);\
         insert into orders values (10, 1, 5), (11, 1, 7), (12, 2, 9);\
         create table pairs (a text, b integer, v text, primary key (a, b));\
         insert into pairs values ('x', 1, 'one'), ('x', 2, 'two');\
         create table logs (msg text);\
         create table files (id integer primary key, data blob);\
         insert into files values (1, x'0102');",
    ]);
    Peer::stdio(fx)
}

fn rows(peer: &mut Peer, sql: &str) -> Json {
    let run = peer.call("q", "query.run", json!({"connection": "App", "sql": sql}));
    run["result"]["ok"]["rows"].clone()
}

#[test]
fn editable_says_what_can_be_edited() {
    let fx = Fixture::new();
    let mut peer = setup(&fx);

    let users = peer.call(1, "table.editable", json!({"connection": "App", "table": "users"}));
    assert_eq!(users["read_only"], Json::Null);
    assert_eq!(users["key"], json!(["id"]));
    assert_eq!(users["table"]["name"], "users");
    assert_eq!(users["columns"][0]["is_primary_key"], true);
    assert_eq!(users["columns"][1]["read_only"], Json::Null);
    let refs = users["referenced_by"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "{users}");
    assert_eq!((&refs[0]["table"], &refs[0]["columns"], &refs[0]["values"]), (&json!("orders"), &json!(["user_id"]), &json!(["id"])));

    let orders = peer.call(2, "table.editable", json!({"connection": "App", "schema": "main", "table": "orders"}));
    let fk = &orders["foreign_keys"][0];
    assert_eq!((&fk["columns"], &fk["table"], &fk["target_columns"]), (&json!(["user_id"]), &json!("users"), &json!(["id"])));
    assert_eq!(orders["columns"][2]["default_value"], "0");

    let view = peer.call(3, "table.editable", json!({"connection": "App", "table": "named"}));
    assert!(view["read_only"].as_str().unwrap().contains("view"), "{view}");
    assert_eq!(view["key"], json!([]));
    let logs = peer.call(4, "table.editable", json!({"connection": "App", "table": "logs"}));
    assert!(logs["read_only"].as_str().unwrap().contains("no primary key"), "{logs}");
    let files = peer.call(5, "table.editable", json!({"connection": "App", "table": "files"}));
    assert_eq!(files["read_only"], Json::Null);
    assert_eq!(files["columns"][1]["binary"], true);
    assert!(files["columns"][1]["read_only"].is_string());
    let pairs = peer.call(6, "table.editable", json!({"connection": "App", "table": "pairs"}));
    assert_eq!(pairs["key"], json!(["a", "b"]));
}

#[test]
fn previews_and_applies_changes_in_one_transaction() {
    let fx = Fixture::new();
    let mut peer = setup(&fx);
    let changes = json!([
        {"kind": "update", "key": [{"column": "id", "value": 2}], "set": [{"column": "note", "value": {"text": "it's"}}, {"column": "score", "value": "null"}]},
        {"kind": "delete", "key": [{"column": "id", "value": 3}]},
        {"kind": "insert", "values": [{"column": "id", "value": "default"}, {"column": "name", "value": {"text": "Cy"}}, {"column": "note", "value": {"text": ""}}]},
    ]);
    let preview = peer.call(1, "table.changes.preview", json!({"connection": "App", "table": "users", "changes": changes}));
    let sql: Vec<&str> = preview["statements"].as_array().unwrap().iter().map(|s| s["sql"].as_str().unwrap()).collect();
    assert_eq!(
        sql,
        [
            r#"DELETE FROM "main"."users" WHERE "id" = 3;"#,
            r#"UPDATE "main"."users" SET "note" = 'it''s', "score" = NULL WHERE "id" = 2;"#,
            r#"INSERT INTO "main"."users" ("name", "note") VALUES ('Cy', '');"#,
        ]
    );
    assert_eq!(preview["statements"][0]["expect_one_row"], true);
    // Previewing writes nothing.
    assert_eq!(rows(&mut peer, "select count(*) from users"), json!([[3]]));

    // The reviewed SQL must be what runs.
    let stale = peer.error(2, "table.changes.apply", json!({"connection": "App", "table": "users", "changes": changes, "reviewed": ["select 1"]}));
    assert!(stale["message"].as_str().unwrap().contains("differ"), "{stale}");
    assert_eq!(rows(&mut peer, "select note from users where id = 2"), json!([["two\nlines"]]));

    let applied = peer.call(3, "table.changes.apply", json!({"connection": "App", "table": "users", "changes": changes, "reviewed": sql}));
    assert_eq!(applied["rows_affected"], 3);
    // SQLite gives the deleted row's id to the new one (deletes run first).
    assert_eq!(
        rows(&mut peer, "select id, name, score, note from users order by id"),
        json!([[1, "Ana", {"float": 1.5}, null], [2, "日本語", null, "it's"], [3, "Cy", null, ""]])
    );
}

#[test]
fn a_failing_change_rolls_back_everything() {
    let fx = Fixture::new();
    let mut peer = setup(&fx);
    // The update would work; the insert reuses a key.
    let changes = json!([
        {"kind": "update", "key": [{"column": "id", "value": 1}], "set": [{"column": "name", "value": {"text": "Changed"}}]},
        {"kind": "insert", "values": [{"column": "id", "value": {"text": "2"}}, {"column": "name", "value": {"text": "Dup"}}]},
    ]);
    let error = peer.error(1, "table.changes.apply", json!({"connection": "App", "table": "users", "changes": changes}));
    assert_eq!(error["data"]["kind"], "query");
    assert!(error["message"].as_str().unwrap().contains("Nothing was saved"), "{error}");
    assert_eq!(rows(&mut peer, "select name from users where id = 1"), json!([["Ana"]]));

    // A row that's gone: the one-row check fails, nothing saved.
    let gone = json!([
        {"kind": "update", "key": [{"column": "id", "value": 1}], "set": [{"column": "name", "value": {"text": "Changed"}}]},
        {"kind": "delete", "key": [{"column": "id", "value": 99}]},
    ]);
    let error = peer.error(2, "table.changes.apply", json!({"connection": "App", "table": "users", "changes": gone}));
    assert!(error["message"].as_str().unwrap().contains("No row matches"), "{error}");
    assert_eq!(rows(&mut peer, "select name from users where id = 1"), json!([["Ana"]]));
}

#[test]
fn refuses_what_the_table_cant_take() {
    let fx = Fixture::new();
    let mut peer = setup(&fx);
    let insert = json!([{"kind": "insert", "values": [{"column": "id", "value": {"text": "9"}}]}]);
    let view = peer.error(1, "table.changes.preview", json!({"connection": "App", "table": "named", "changes": insert}));
    assert_eq!(view["data"]["kind"], "unsupported");
    let logs = peer.error(2, "table.changes.apply", json!({"connection": "App", "table": "logs", "changes": [{"kind": "insert", "values": [{"column": "msg", "value": {"text": "x"}}]}]}));
    assert_eq!(logs["data"]["kind"], "unsupported");
    assert_eq!(rows(&mut peer, "select count(*) from logs"), json!([[0]]));

    // Binary cells: never from text; NULL is fine.
    let blob = |value: Json| json!([{"kind": "update", "key": [{"column": "id", "value": 1}], "set": [{"column": "data", "value": value}]}]);
    let text = peer.error(3, "table.changes.preview", json!({"connection": "App", "table": "files", "changes": blob(json!({"text": "0x0102"}))}));
    assert!(text["message"].as_str().unwrap().contains("Binary"), "{text}");
    let null = peer.call(4, "table.changes.preview", json!({"connection": "App", "table": "files", "changes": blob(json!("null"))}));
    assert_eq!(null["statements"][0]["sql"], r#"UPDATE "main"."files" SET "data" = NULL WHERE "id" = 1;"#);

    // A composite key must be whole.
    let partial = json!([{"kind": "delete", "key": [{"column": "a", "value": "x"}]}]);
    let error = peer.error(5, "table.changes.apply", json!({"connection": "App", "table": "pairs", "changes": partial}));
    assert!(error["message"].as_str().unwrap().contains("whole primary key"), "{error}");
    let whole = json!([{"kind": "delete", "key": [{"column": "b", "value": 2}, {"column": "a", "value": "x"}]}]);
    assert_eq!(peer.call(6, "table.changes.apply", json!({"connection": "App", "table": "pairs", "changes": whole}))["rows_affected"], 1);
    assert_eq!(rows(&mut peer, "select a, b from pairs"), json!([["x", 1]]));

    let unknown = peer.error(7, "table.changes.preview", json!({"connection": "App", "table": "users", "changes": [{"kind": "insert", "values": [{"column": "nope", "value": "null"}]}]}));
    assert!(unknown["message"].as_str().unwrap().contains("nope"), "{unknown}");
    let bad = peer.error(8, "table.changes.preview", json!({"connection": "App", "table": "users", "changes": [{"kind": "rename"}]}));
    assert_eq!(bad["code"], -32602);
}

#[test]
fn related_rows_filter() {
    let fx = Fixture::new();
    let mut peer = setup(&fx);
    let users = peer.call(1, "table.related", json!({"connection": "App", "table": "users", "columns": ["id"], "values": [1]}));
    assert_eq!(users["filter"], r#""id" = 1"#);
    assert_eq!(users["table"]["schema"], "main");
    let orders = peer.call(2, "table.related", json!({"connection": "App", "table": "orders", "columns": ["user_id"], "values": [1]}));
    let filter = orders["filter"].as_str().unwrap();
    let page = peer.call(3, "table.rows", json!({"connection": "App", "table": "orders", "query": {"filter": filter}}));
    assert_eq!(page["result"]["rows"], json!([[10, 1, 5], [11, 1, 7]]));
    // No columns: the table's primary key.
    let by_key = peer.call(4, "table.related", json!({"connection": "App", "table": "pairs", "values": ["x", 2]}));
    assert_eq!(by_key["filter"], r#""a" = 'x' and "b" = 2"#);
    let text = peer.call(5, "table.related", json!({"connection": "App", "table": "users", "columns": ["note"], "values": [null]}));
    assert_eq!(text["filter"], r#""note" is null"#);
}
