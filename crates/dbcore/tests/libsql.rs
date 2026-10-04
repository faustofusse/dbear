//! Integration tests against the dev libSQL server (`scripts/dev-db.sh up libsql`, container `dbear-libsql`).
//! Skipped unless `DBEAR_TEST_LIBSQL=1`; `scripts/test-libsql.sh` sets it up.
//!
//! The server is shared and seeded once from `dev/sqlite/init.sql`: tests only read the seed tables
//! and write to scratch tables of their own.

use std::time::{Duration, Instant};

use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{mock, Connection, ConnectionConfig, Error, RowQuery, SortKey, TableInfo, TableKind, Value};

fn enabled() -> bool {
    let on = std::env::var("DBEAR_TEST_LIBSQL").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set DBEAR_TEST_LIBSQL=1 (scripts/dev-db.sh up libsql)");
    }
    on
}

fn dev_config() -> ConnectionConfig {
    mock::connections().into_iter().find(|c| c.id == mock::DEV_LIBSQL).unwrap()
}

fn dev() -> Connection {
    Connection::new(dev_config())
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn column<'a>(result: &'a dbcore::QueryResult, name: &str) -> Vec<&'a Value> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| &r[i]).collect()
}

/// A scratch table name unique to this test run.
fn scratch(name: &str) -> String {
    format!("{name}_{}", std::process::id())
}

#[test]
fn connects_and_rejects_bad_tokens() {
    if !enabled() {
        return;
    }
    let conn = dev();
    assert!(!block_on(conn.is_connected()));
    block_on(conn.connect()).unwrap();
    assert!(block_on(conn.is_connected()));
    block_on(conn.disconnect());
    assert!(!block_on(conn.is_connected()));

    let mut bad = dev_config();
    bad.password = Some("not-a-token".into());
    let err = block_on(Connection::new(bad.clone()).connect()).unwrap_err();
    assert!(matches!(&err, Error::ConnectionFailed(m) if m.contains("auth token")), "{err:?}");
    bad.password = None;
    assert!(matches!(block_on(Connection::new(bad).connect()), Err(Error::ConnectionFailed(_))));

    let mut nowhere = dev_config();
    nowhere.port = Some(1);
    assert!(matches!(block_on(Connection::new(nowhere).connect()), Err(Error::ConnectionFailed(_))));
}

#[test]
fn lists_tables_without_counts_and_columns() {
    if !enabled() {
        return;
    }
    let schemas = block_on(dev().list_schemas()).unwrap();
    assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"]);
    let tables: Vec<_> = schemas[0].tables.iter().map(|t| (t.name.as_str(), t.kind, t.estimated_row_count)).collect();
    for expected in [
        ("events", TableKind::Table, None),
        ("note_tags", TableKind::Table, None),
        ("notes", TableKind::Table, None),
        ("pinned_notes", TableKind::View, None),
        ("settings", TableKind::Table, None),
        ("tags", TableKind::Table, None),
    ] {
        assert!(tables.contains(&expected), "{expected:?} in {tables:?}");
    }

    let columns = block_on(dev().list_columns()).unwrap();
    let notes = columns.iter().find(|t| t.schema == "main" && t.table == "notes").expect("notes");
    let names: Vec<_> = notes.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "title", "body", "pinned", "word_count", "score", "price", "attachment", "created_at"]);
    assert!(notes.columns[0].is_primary_key && notes.columns[2].is_nullable);
}

#[test]
fn pages_tables_with_typed_values_and_no_counts() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let first = block_on(conn.fetch_rows(TableInfo::new("main", "notes"), 50, 0)).unwrap();
    // Never counted: `count(*)` would read (and bill) every row.
    assert_eq!((first.rows.len(), first.total_count), (50, None));
    let id = first.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.is_primary_key && !id.is_nullable);
    assert_eq!(first.columns.iter().find(|c| c.name == "price").unwrap().type_name, "decimal(10,2)");
    assert_eq!(column(&first, "id")[..3], [&Value::Int(1), &Value::Int(2), &Value::Int(3)]);
    assert_eq!(column(&first, "pinned")[3], &Value::Bool(true));
    assert_eq!(column(&first, "price")[2], &Value::Decimal("19.9".into()));
    assert_eq!(column(&first, "body")[5], &Value::Null);
    assert!(column(&first, "body")[0].display().contains("🎉"));
    assert!(matches!(column(&first, "attachment")[9], Value::Text(t) if t.starts_with("0x") && t.len() == 18));

    let last = block_on(conn.fetch_rows(TableInfo::new("main", "notes"), 50, 100)).unwrap();
    assert_eq!((last.rows.len(), last.total_count), (20, None));
    assert_eq!(column(&last, "id")[0], &Value::Int(101));
}

#[test]
fn pages_without_rowid_and_keyless_tables_and_views() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let tags = block_on(conn.fetch_rows(TableInfo::new("main", "note_tags"), 3, 0)).unwrap();
    assert_eq!(tags.rows[0][..2], [Value::Int(1), Value::Int(2)]);
    let settings = block_on(conn.fetch_rows(TableInfo::new("main", "settings"), 10, 0)).unwrap();
    assert_eq!(
        column(&settings, "value"),
        [&Value::Text("dark".into()), &Value::Int(13), &Value::Float(1.25), &Value::Null, &Value::Text("0xdeadbeef".into())]
    );
    let view = block_on(conn.fetch_rows(TableInfo::new("main", "pinned_notes"), 100, 0)).unwrap();
    assert_eq!((view.rows.len(), view.total_count), (30, None));

    let err = block_on(conn.fetch_rows(TableInfo::new("main", "nope"), 10, 0)).unwrap_err();
    assert_eq!(err, Error::TableNotFound("main.nope".into()));
}

#[test]
fn sorts_and_filters() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let notes = TableInfo::new("main", "notes");
    let query = RowQuery { sort: vec![SortKey { column: "title".into(), descending: true }], filter: Some("pinned".into()) };
    let page = block_on(conn.fetch_rows_with(notes.clone(), query, 1000, 0)).unwrap();
    let titles: Vec<String> = column(&page, "title").iter().map(|v| v.display()).collect();
    assert_eq!(titles.len(), 30);
    assert!(titles.windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(page.total_count, None);

    let bad = RowQuery { filter: Some("nope = 1".into()), ..Default::default() };
    let err = block_on(conn.fetch_rows_with(notes.clone(), bad, 10, 0)).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("nope") && !m.contains("line")), "{err:?}");
    let evil = RowQuery { filter: Some("1; delete from notes".into()), ..Default::default() };
    assert!(block_on(conn.fetch_rows_with(notes.clone(), evil, 10, 0)).is_err());
    let sort_missing = RowQuery { sort: vec![SortKey { column: "nope".into(), descending: false }], ..Default::default() };
    assert!(block_on(conn.fetch_rows_with(notes.clone(), sort_missing, 10, 0)).is_err());
    assert_eq!(block_on(conn.fetch_rows(notes, 200, 0)).unwrap().rows.len(), 120);
}

#[test]
fn describes_tables() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let links = block_on(conn.describe_table(TableInfo::new("main", "note_tags"))).unwrap();
    assert_eq!(links.primary_key, ["note_id", "tag_id"]);
    assert_eq!(links.foreign_keys.len(), 2);
    let to_notes = links.foreign_keys.iter().find(|f| f.referenced_table == "notes").unwrap();
    assert_eq!((to_notes.columns.as_slice(), to_notes.on_delete.as_str()), (&["note_id".to_string()][..], "CASCADE"));
    assert!(links.ddl.unwrap().to_lowercase().contains("without rowid"));
    assert!(links.indexes.iter().any(|i| i.is_primary && i.columns == ["note_id", "tag_id"]));

    let tags = block_on(conn.describe_table(TableInfo::new("main", "tags"))).unwrap();
    let incoming: Vec<_> = tags.referenced_by.iter().map(|k| (k.table.as_str(), k.columns.clone(), k.referenced_columns.clone())).collect();
    assert_eq!(incoming, [("note_tags", vec!["tag_id".to_string()], vec!["id".to_string()])]);
    assert!(tags.indexes.iter().any(|i| i.is_unique && i.columns == ["name"] && i.definition.is_none()));
    let notes = block_on(conn.describe_table(TableInfo::new("main", "notes"))).unwrap();
    let pinned = notes.columns.iter().find(|c| c.name == "pinned").unwrap();
    assert_eq!(pinned.default_value.as_deref(), Some("0"));
    assert_eq!(notes.primary_key, ["id"]);
    let view = block_on(conn.describe_table(TableInfo::new("main", "pinned_notes"))).unwrap();
    assert!(view.ddl.unwrap().to_lowercase().starts_with("create view"));
    assert_eq!(view.columns.len(), 3);
    assert!(matches!(block_on(conn.describe_table(TableInfo::new("main", "nope"))), Err(Error::TableNotFound(_))));
}

#[test]
fn runs_scripts() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let t = scratch("script_test");
    // Several statements (with a trigger whose body has `;`s): last result set wins.
    let r = block_on(conn.execute(format!(
        "drop table if exists {t}; drop table if exists {t}_log;
         create table {t} (id integer primary key, name text);
         create table {t}_log (msg text);
         create trigger {t}_ins after insert on {t} begin
           insert into {t}_log values ('added ' || new.name);
           insert into {t}_log values ('again; with a semicolon');
         end;
         insert into {t} (name) values ('a'), ('b');
         select count(*) as n from {t}_log; -- trailing comment"
    )))
    .unwrap();
    assert_eq!((r.columns[0].name.as_str(), r.rows.clone()), ("n", vec![vec![Value::Int(4)]]));

    let r = block_on(conn.execute(format!("insert into {t} (name) values ('c'), ('d'), ('e')"))).unwrap();
    assert_eq!((r.rows_affected, r.columns.len()), (Some(3), 0));

    let r = block_on(conn.execute_limited("select * from events".into(), Some(1000))).unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.total_count), (1000, true, Some(20_000)));

    // Empty results keep their columns; empty scripts are fine.
    let r = block_on(conn.execute(format!("select id, name from {t} where 0"))).unwrap();
    assert_eq!((r.columns.len(), r.rows.len()), (2, 0));
    assert_eq!(block_on(conn.execute(" -- nothing\n".into())).unwrap().rows_affected, None);

    // A transaction a script leaves open is rolled back when the script ends.
    block_on(conn.execute(format!("begin; delete from {t};"))).unwrap();
    let r = block_on(conn.execute(format!("select count(*) from {t}"))).unwrap();
    assert_eq!(r.rows, [[Value::Int(5)]]);

    block_on(conn.execute(format!("drop table {t}; drop table {t}_log"))).unwrap();
}

#[test]
fn reports_errors_with_position_and_stops() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let t = scratch("error_test");
    block_on(conn.execute(format!("drop table if exists {t}; create table {t} (x)"))).unwrap();
    let err = block_on(conn.execute(format!("insert into {t} values (1);\n  select nope from notes;\ninsert into {t} values (2)")))
        .unwrap_err();
    let Error::Query(message) = err else { panic!("{err:?}") };
    assert!(message.contains("no such column: nope") && message.contains("line 2, column 3"), "{message}");
    // Statements before the error ran; the ones after didn't.
    let r = block_on(conn.execute(format!("select x from {t}"))).unwrap();
    assert_eq!(r.rows, [[Value::Int(1)]]);
    block_on(conn.execute(format!("drop table {t}"))).unwrap();
}

#[test]
fn cancels_long_scripts() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let started = Instant::now();
    let runner = conn.clone();
    // Streams rows (bounded, so the server stops on its own too).
    let handle = std::thread::spawn(move || {
        block_on(runner.execute_limited(
            "with recursive r(n) as (select 1 union all select n + 1 from r where n < 50000000) select n from r".into(),
            Some(10),
        ))
    });
    std::thread::sleep(Duration::from_millis(500));
    block_on(conn.cancel());
    assert_eq!(handle.join().unwrap().unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(block_on(conn.execute("select 1".into())).unwrap().rows, [[Value::Int(1)]]);
}

// MARK: Editing

fn edit_key(id: i64) -> Vec<KeyValue> {
    vec![KeyValue { column: "id".into(), value: Value::Int(id) }]
}

fn edit_set(column: &str, value: EditValue) -> CellEdit {
    CellEdit { column: column.into(), value }
}

fn edit_text(s: &str) -> EditValue {
    EditValue::Text(s.into())
}

#[test]
fn saves_row_edits_in_one_transaction() {
    if !enabled() {
        return;
    }
    let conn = dev();
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
        RowChange::Update { key: edit_key(1), set: vec![edit_set("name", edit_text("Ada")), edit_set("n", EditValue::Null), edit_set("active", edit_text("false"))] },
        RowChange::Delete { key: edit_key(2) },
        RowChange::Insert { values: vec![edit_set("id", EditValue::Default), edit_set("name", edit_text("Grace")), edit_set("n", edit_text("7"))] },
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
        RowChange::Update { key: edit_key(3), set: vec![edit_set("name", edit_text("changed"))] },
        RowChange::Update { key: edit_key(2), set: vec![edit_set("name", edit_text("ghost"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("No row matches") && m.contains("Nothing was saved")), "{err:?}");
    assert_eq!(names(), ["Ada", "c", "Grace"]);

    // Server errors (unique violation) name the row and roll everything back.
    let err = apply(vec![
        RowChange::Update { key: edit_key(3), set: vec![edit_set("name", edit_text("changed"))] },
        RowChange::Insert { values: vec![edit_set("name", edit_text("Ada"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.starts_with("Couldn’t save a new row") && m.ends_with("Nothing was saved.")), "{err:?}");
    assert_eq!(names(), ["Ada", "c", "Grace"]);

    block_on(conn.execute(format!("drop table {t}"))).unwrap();
}
