//! Integration tests against a real Cloudflare D1 database (there's no local server that speaks
//! D1's REST API). Skipped unless `DBEAR_TEST_D1_URL` is set to `d1://<account>/<database>?token=…`
//! for a scratch database: tests write tables of their own (named after the process) and drop them.

use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Error, PageCursor, RowQuery, SortKey, TableInfo, TableKind, Value};

fn config() -> Option<ConnectionConfig> {
    match std::env::var("DBEAR_TEST_D1_URL") {
        Ok(url) if !url.is_empty() => Some(ConnectionConfig::from_url(&url).expect("DBEAR_TEST_D1_URL is a d1:// URL")),
        _ => {
            eprintln!("skipped: set DBEAR_TEST_D1_URL=d1://<account>/<database>?token=… (a scratch database)");
            None
        }
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn column<'a>(result: &'a dbcore::QueryResult, name: &str) -> Vec<&'a Value> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| &r[i]).collect()
}

fn scratch(name: &str) -> String {
    format!("{name}_{}", std::process::id())
}

#[test]
fn connects_lists_and_rejects_bad_tokens() {
    let Some(config) = config() else { return };
    assert_eq!(config.kind, DatabaseKind::D1);
    let conn = Connection::new(config.clone());
    block_on(conn.connect()).unwrap();
    assert!(block_on(conn.is_connected()));
    let databases = block_on(conn.list_databases()).unwrap();
    assert!(!databases.is_empty() && databases.windows(2).all(|w| w[0] <= w[1]), "{databases:?}");

    // Internal tables (`_cf_KV`) are hidden.
    let schemas = block_on(conn.list_schemas()).unwrap();
    assert!(schemas[0].tables.iter().all(|t| !t.name.starts_with("_cf_")));
    block_on(conn.list_columns()).unwrap();

    // By name instead of ID, and without the account (the token reaches one).
    if !config.host.is_empty() {
        let mut by_name = config.clone();
        by_name.host.clear();
        block_on(Connection::new(by_name).connect()).unwrap();
    }

    let mut bad = config.clone();
    bad.password = Some("not-a-token".into());
    let err = block_on(Connection::new(bad).connect()).unwrap_err();
    assert!(matches!(&err, Error::ConnectionFailed(m) if m.contains("API token")), "{err:?}");
    let mut missing = config;
    missing.database = "dbear-no-such-database".into();
    let err = block_on(Connection::new(missing).connect()).unwrap_err();
    assert!(matches!(&err, Error::ConnectionFailed(m) if m.contains("no D1 database named")), "{err:?}");
}

#[test]
fn pages_tables_with_exact_values() {
    let Some(config) = config() else { return };
    let conn = Connection::new(config);
    let t = scratch("values_test");
    let r = block_on(conn.execute(format!(
        "drop table if exists {t};
         create table {t} (id integer primary key, name text not null, big integer, r real, b blob, flag boolean, price decimal(10,2));
         insert into {t} values (1, 'it''s', 9007199254740993, 1.0, x'00ff10', 1, 19.9), (2, '🎉', -3, 0.1, null, 0, null);
         insert into {t} (id, name) select value + 2, 'n' || value from json_each('[' || (select group_concat(n) from (with recursive c(n) as (select 1 union all select n + 1 from c where n < 120) select n from c)) || ']');
         select count(*) as n from {t}"
    )))
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(122)]]);

    let table = TableInfo::new("main", &t);
    let first = block_on(conn.fetch_rows(table.clone(), 50, 0)).unwrap();
    assert_eq!((first.rows.len(), first.total_count), (50, None));
    assert_eq!(column(&first, "big")[0], &Value::Int(9007199254740993));
    assert_eq!(column(&first, "r")[0], &Value::Float(1.0));
    assert_eq!(column(&first, "name")[0], &Value::Text("it's".into()));
    assert_eq!(column(&first, "name")[1], &Value::Text("🎉".into()));
    assert_eq!(column(&first, "b")[0], &Value::Text("0x00ff10".into()));
    assert_eq!(column(&first, "flag")[..2], [&Value::Bool(true), &Value::Bool(false)]);
    assert_eq!(column(&first, "price")[0], &Value::Decimal("19.9".into()));
    assert!(first.columns[0].is_primary_key);

    // Keyset pages: every row once, in order.
    let mut ids = Vec::new();
    let mut after: Option<PageCursor> = None;
    loop {
        let page = block_on(conn.fetch_page(table.clone(), RowQuery::default(), 50, after.clone())).unwrap();
        ids.extend(column(&page.result, "id").into_iter().cloned());
        if page.next.is_none() {
            break;
        }
        assert!(page.next.as_ref().unwrap().is_keyset());
        after = page.next;
    }
    assert_eq!(ids, (1..=122).map(Value::Int).collect::<Vec<_>>());

    // Sorted by text, descending, with a filter.
    let query = RowQuery { sort: vec![SortKey { column: "name".into(), descending: true }], filter: Some("id > 2".into()) };
    let mut names = Vec::new();
    let mut after: Option<PageCursor> = None;
    loop {
        let page = block_on(conn.fetch_page(table.clone(), query.clone(), 7, after.clone())).unwrap();
        names.extend(column(&page.result, "name").iter().map(|v| v.display()));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(names.len(), 120);
    assert!(names.windows(2).all(|w| w[0] >= w[1]));

    let err = block_on(conn.fetch_rows(TableInfo::new("main", "nope"), 10, 0)).unwrap_err();
    assert_eq!(err, Error::TableNotFound("main.nope".into()));
    block_on(conn.execute(format!("drop table {t}"))).unwrap();
}

#[test]
fn describes_tables_and_views() {
    let Some(config) = config() else { return };
    let conn = Connection::new(config);
    let (p, c, v) = (scratch("parent_test"), scratch("child_test"), scratch("view_test"));
    block_on(conn.execute(format!(
        "drop view if exists {v}; drop table if exists {c}; drop table if exists {p};
         create table {p} (id integer primary key, name text unique, n integer default 0);
         create table {c} (a integer, b integer references {p}(id) on delete cascade, primary key (a, b)) without rowid;
         create index {c}_b on {c}(b);
         create view {v} as select id, name from {p};"
    )))
    .unwrap();
    let parent = block_on(conn.describe_table(TableInfo::new("main", &p))).unwrap();
    assert_eq!(parent.primary_key, ["id"]);
    assert_eq!(parent.columns.iter().find(|c| c.name == "n").unwrap().default_value.as_deref(), Some("0"));
    assert_eq!(parent.referenced_by.iter().map(|k| k.table.as_str()).collect::<Vec<_>>(), [c.as_str()]);
    assert!(parent.indexes.iter().any(|i| i.is_unique && i.columns == ["name"]));
    let child = block_on(conn.describe_table(TableInfo::new("main", &c))).unwrap();
    assert_eq!(child.primary_key, ["a", "b"]);
    assert_eq!((child.foreign_keys.len(), child.foreign_keys[0].on_delete.as_str()), (1, "CASCADE"));
    assert!(child.ddl.unwrap().to_lowercase().contains("without rowid"));
    let view = block_on(conn.describe_table(TableInfo::new("main", &v))).unwrap();
    assert_eq!(view.columns.len(), 2);

    let schemas = block_on(conn.list_schemas()).unwrap();
    assert!(schemas[0].tables.iter().any(|t| t.name == v && t.kind == TableKind::View));
    block_on(conn.execute(format!("drop view {v}; drop table {c}; drop table {p}"))).unwrap();
}

#[test]
fn runs_scripts_all_or_nothing() {
    let Some(config) = config() else { return };
    let conn = Connection::new(config);
    let t = scratch("script_test");
    let r = block_on(conn.execute(format!(
        "drop table if exists {t};
         create table {t} (id integer primary key, name text);
         create trigger {t}_ins after insert on {t} begin
           update {t} set name = upper(new.name) where id = new.id;
         end;
         insert into {t} (name) values ('a'), ('b');
         select name from {t} order by id; -- trailing comment"
    )))
    .unwrap();
    assert_eq!(r.rows, [[Value::Text("A".into())], [Value::Text("B".into())]]);

    let r = block_on(conn.execute(format!("insert into {t} (name) values ('c'), ('d'), ('e')"))).unwrap();
    assert_eq!((r.rows_affected, r.columns.len()), (Some(3), 0));
    let r = block_on(conn.execute_limited(format!("select * from {t}"), Some(2))).unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.total_count), (2, true, Some(5)));
    let r = block_on(conn.execute(format!("select id, name from {t} where 0"))).unwrap();
    assert_eq!((r.columns.len(), r.rows.len()), (2, 0));
    assert_eq!(block_on(conn.execute(" -- nothing\n".into())).unwrap().rows_affected, None);

    // One statement: the error says where.
    let err = block_on(conn.execute("\n  selec 1".into())).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("syntax error") && m.contains("line 2, column 3")), "{err:?}");
    // A failing statement rolls back the whole script.
    let err = block_on(conn.execute(format!("delete from {t}; select nope from {t}"))).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("no such column: nope")), "{err:?}");
    let r = block_on(conn.execute(format!("select count(*) from {t}"))).unwrap();
    assert_eq!(r.rows, [[Value::Int(5)]]);
    block_on(conn.execute(format!("drop table {t}"))).unwrap();
}

fn key(id: i64) -> Vec<KeyValue> {
    vec![KeyValue { column: "id".into(), value: Value::Int(id) }]
}

fn set(column: &str, value: EditValue) -> CellEdit {
    CellEdit { column: column.into(), value }
}

fn text(s: &str) -> EditValue {
    EditValue::Text(s.into())
}

#[test]
fn saves_row_edits_in_one_batch() {
    let Some(config) = config() else { return };
    let conn = Connection::new(config);
    let t = scratch("edit_test");
    block_on(conn.execute(format!(
        "drop table if exists {t};
         create table {t} (id integer primary key, name text not null unique, n integer,
           active boolean not null default 1, note text default 'hi');
         insert into {t} (id, name, n) values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3);"
    )))
    .unwrap();
    let table = TableInfo::new("main", &t);
    let names = || -> Vec<String> {
        let page = block_on(conn.fetch_rows(table.clone(), 100, 0)).unwrap();
        column(&page, "name").iter().map(|v| v.display()).collect()
    };
    let columns = block_on(conn.fetch_rows(table.clone(), 1, 0)).unwrap().columns;
    let apply = |changes: Vec<RowChange>| block_on(conn.apply_changes(table.clone(), columns.clone(), changes));

    let affected = apply(vec![
        RowChange::Update { key: key(1), set: vec![set("name", text("Ada")), set("n", EditValue::Null), set("active", text("false"))] },
        RowChange::Delete { key: key(2) },
        RowChange::Insert { values: vec![set("id", EditValue::Default), set("name", text("Grace")), set("n", text("7"))] },
    ])
    .unwrap();
    assert_eq!(affected, 3);
    assert_eq!(names(), ["Ada", "c", "Grace"]);
    let page = block_on(conn.fetch_rows(table.clone(), 100, 0)).unwrap();
    assert_eq!(column(&page, "n")[0], &Value::Null);
    assert_eq!(column(&page, "note")[2].display(), "hi");
    assert_eq!(column(&page, "active")[0], &Value::Bool(false));

    // A vanished row fails the whole batch: the other update is rolled back too.
    let err = apply(vec![
        RowChange::Update { key: key(3), set: vec![set("name", text("changed"))] },
        RowChange::Update { key: key(2), set: vec![set("name", text("ghost"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("No row matches") && m.contains("Nothing was saved")), "{err:?}");
    assert_eq!(names(), ["Ada", "c", "Grace"]);

    // Server errors (unique violation) roll everything back.
    let err = apply(vec![
        RowChange::Update { key: key(3), set: vec![set("name", text("changed"))] },
        RowChange::Insert { values: vec![set("name", text("Ada"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("UNIQUE constraint failed") && m.ends_with("Nothing was saved.")), "{err:?}");
    assert_eq!(names(), ["Ada", "c", "Grace"]);

    block_on(conn.execute(format!("drop table {t}"))).unwrap();
}
