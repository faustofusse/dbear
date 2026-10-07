//! SQLite driver tests. Always run: each test seeds its own temporary file from `dev/sqlite/init.sql`.

use std::time::{Duration, Instant};

use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{Connection, ConnectionConfig, DatabaseKind, Error, RowQuery, SortKey, TableInfo, TableKind, Value};

struct TempDb {
    _dir: tempfile::TempDir,
    path: String,
}

fn seeded() -> TempDb {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let seed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dev/sqlite/init.sql")).unwrap();
    rusqlite::Connection::open(&path).unwrap().execute_batch(&seed).unwrap();
    TempDb { path: path.display().to_string(), _dir: dir }
}

fn open(db: &TempDb) -> Connection {
    let mut config = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
    config.id = "test".into();
    config.database = db.path.clone();
    Connection::new(config)
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn column<'a>(result: &'a dbcore::QueryResult, name: &str) -> Vec<&'a Value> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| &r[i]).collect()
}

#[test]
fn lists_columns_of_every_table() {
    let db = seeded();
    let tables = block_on(open(&db).list_columns()).unwrap();
    let notes = tables.iter().find(|t| t.schema == "main" && t.table == "notes").expect("notes");
    let names: Vec<_> = notes.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "title", "body", "pinned", "word_count", "score", "price", "attachment", "created_at"], "{names:?}");
    let id = notes.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.is_primary_key);
    let body = notes.columns.iter().find(|c| c.name == "body").unwrap();
    assert!(body.is_nullable);
}

#[test]
fn lists_tables_and_views_with_counts() {
    let db = seeded();
    let schemas = block_on(open(&db).list_schemas()).unwrap();
    assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"]);
    let tables: Vec<_> = schemas[0].tables.iter().map(|t| (t.name.as_str(), t.kind, t.estimated_row_count)).collect();
    assert_eq!(
        tables,
        [
            ("events", TableKind::Table, Some(20_000)),
            ("note_tags", TableKind::Table, Some(168)),
            ("notes", TableKind::Table, Some(120)),
            ("pinned_notes", TableKind::View, None),
            ("settings", TableKind::Table, Some(5)),
            ("tags", TableKind::Table, Some(5)),
        ]
    );
}

#[test]
fn pages_tables_with_typed_values() {
    let db = seeded();
    let conn = open(&db);
    let first = block_on(conn.fetch_rows(TableInfo::new("main", "notes"), 50, 0)).unwrap();
    assert_eq!((first.rows.len(), first.total_count), (50, Some(120)));
    let id = first.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.is_primary_key && !id.is_nullable);
    assert_eq!(first.columns.iter().find(|c| c.name == "price").unwrap().type_name, "decimal(10,2)");
    assert_eq!(column(&first, "id")[..3], [&Value::Int(1), &Value::Int(2), &Value::Int(3)]);
    assert_eq!(column(&first, "pinned")[3], &Value::Bool(true));
    assert_eq!(column(&first, "price")[2], &Value::Decimal("19.9".into()));
    assert_eq!(column(&first, "body")[5], &Value::Null);
    assert!(matches!(column(&first, "attachment")[9], Value::Text(t) if t.starts_with("0x") && t.len() == 18));

    let last = block_on(conn.fetch_rows(TableInfo::new("main", "notes"), 50, 100)).unwrap();
    assert_eq!((last.rows.len(), last.total_count), (20, None));
    assert_eq!(column(&last, "id")[0], &Value::Int(101));
}

#[test]
fn pages_without_rowid_and_keyless_tables_and_views() {
    let db = seeded();
    let conn = open(&db);
    let tags = block_on(conn.fetch_rows(TableInfo::new("main", "note_tags"), 3, 0)).unwrap();
    assert_eq!(tags.rows[0][..2], [Value::Int(1), Value::Int(2)]);
    let settings = block_on(conn.fetch_rows(TableInfo::new("main", "settings"), 10, 0)).unwrap();
    assert_eq!(
        column(&settings, "value"),
        [&Value::Text("dark".into()), &Value::Int(13), &Value::Float(1.25), &Value::Null, &Value::Text("0xdeadbeef".into())]
    );
    let view = block_on(conn.fetch_rows(TableInfo::new("main", "pinned_notes"), 100, 0)).unwrap();
    assert_eq!((view.rows.len(), view.total_count), (30, None));
}

#[test]
fn missing_tables_and_files_fail_clearly() {
    let db = seeded();
    let err = block_on(open(&db).fetch_rows(TableInfo::new("main", "nope"), 10, 0)).unwrap_err();
    assert_eq!(err, Error::TableNotFound("main.nope".into()));

    let missing = TempDb { path: "/definitely/not/here.db".into(), _dir: tempfile::tempdir().unwrap() };
    let err = block_on(open(&missing).connect()).unwrap_err();
    assert!(matches!(err, Error::ConnectionFailed(ref m) if m.contains("No database file")), "{err:?}");
}

#[test]
fn runs_scripts() {
    let db = seeded();
    let conn = open(&db);
    // Last statement's rows win; writes before it are applied.
    let r = block_on(conn.execute(
        "update notes set pinned = 1 where id <= 3; select count(*) as n from notes where pinned".into(),
    ))
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(33)]]);

    let r = block_on(conn.execute("insert into tags (name) values ('a'), ('b')".into())).unwrap();
    assert_eq!((r.rows_affected, r.columns.len()), (Some(2), 0));

    let r = block_on(conn.execute_limited("select * from events".into(), Some(1000))).unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.total_count), (1000, true, Some(20_000)));

    // Attached databases show up as schemas.
    block_on(conn.execute("attach ':memory:' as scratch; create table scratch.t (x)".into())).unwrap();
    let schemas = block_on(conn.list_schemas()).unwrap();
    assert!(schemas.iter().any(|s| s.name == "main"));
}

#[test]
fn reports_errors_with_position() {
    let db = seeded();
    let err = block_on(open(&db).execute("select 1;\nselect nope from notes".into())).unwrap_err();
    let Error::Query(message) = err else { panic!("{err:?}") };
    assert!(message.contains("no such column: nope") && message.contains("line 2, column 8"), "{message}");
}

#[test]
fn cancels_long_scripts() {
    let db = seeded();
    let conn = open(&db);
    let started = Instant::now();
    let runner = conn.clone();
    let handle = std::thread::spawn(move || {
        block_on(runner.execute(
            "with recursive r(n) as (select 1 union all select n + 1 from r) select count(*) from r".into(),
        ))
    });
    std::thread::sleep(Duration::from_millis(300));
    block_on(conn.cancel());
    assert_eq!(handle.join().unwrap().unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    // The session is still usable.
    assert_eq!(block_on(conn.execute("select 1".into())).unwrap().rows, [[Value::Int(1)]]);
}

#[test]
fn tracks_connection_state() {
    let db = seeded();
    let conn = open(&db);
    assert!(!block_on(conn.is_connected()));
    block_on(conn.list_schemas()).unwrap();
    assert!(block_on(conn.is_connected()));
    block_on(conn.disconnect());
    assert!(!block_on(conn.is_connected()));
}

#[test]
fn sorts_and_filters() {
    let db = seeded();
    let conn = open(&db);
    let notes = TableInfo::new("main", "notes");
    let query = RowQuery { sort: vec![SortKey { column: "title".into(), descending: false }], filter: Some("pinned".into()) };
    let page = block_on(conn.fetch_rows_with(notes.clone(), query, 1000, 0)).unwrap();
    let titles: Vec<String> = column(&page, "title").iter().map(|v| v.display()).collect();
    assert!(!titles.is_empty() && titles.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(page.total_count, Some(titles.len() as u64));

    let bad = RowQuery { filter: Some("nope = 1".into()), ..Default::default() };
    let err = block_on(conn.fetch_rows_with(notes.clone(), bad, 10, 0)).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("nope") && !m.contains("line")), "{err:?}");
    let evil = RowQuery { filter: Some("1; delete from notes".into()), ..Default::default() };
    assert!(block_on(conn.fetch_rows_with(notes.clone(), evil, 10, 0)).is_err());
    assert_eq!(block_on(conn.fetch_rows(notes, 1, 0)).unwrap().total_count, Some(120));
}

#[test]
fn describes_tables() {
    let db = seeded();
    let conn = open(&db);
    let links = block_on(conn.describe_table(TableInfo::new("main", "note_tags"))).unwrap();
    assert_eq!(links.primary_key, ["note_id", "tag_id"]);
    assert_eq!(links.foreign_keys.len(), 2);
    let to_notes = links.foreign_keys.iter().find(|f| f.referenced_table == "notes").unwrap();
    assert_eq!((to_notes.columns.as_slice(), to_notes.on_delete.as_str()), (&["note_id".to_string()][..], "CASCADE"));
    assert!(links.ddl.unwrap().to_lowercase().contains("without rowid"));

    let tags = block_on(conn.describe_table(TableInfo::new("main", "tags"))).unwrap();
    assert!(tags.indexes.iter().any(|i| i.is_unique && i.columns == ["name"]));
    let incoming: Vec<_> = tags.referenced_by.iter().map(|k| (k.table.as_str(), k.columns.clone(), k.referenced_columns.clone())).collect();
    assert_eq!(incoming, [("note_tags", vec!["tag_id".to_string()], vec!["id".to_string()])]);
    // A key without columns points at the primary key: no referenced columns.
    block_on(conn.execute("create table tag_aliases (alias text, tag integer references tags)".into())).unwrap();
    let tags = block_on(conn.describe_table(TableInfo::new("main", "tags"))).unwrap();
    let alias = tags.referenced_by.iter().find(|k| k.table == "tag_aliases").unwrap();
    assert_eq!((alias.columns.as_slice(), alias.referenced_columns.is_empty()), (&["tag".to_string()][..], true));
    let notes = block_on(conn.describe_table(TableInfo::new("main", "notes"))).unwrap();
    let pinned = notes.columns.iter().find(|c| c.name == "pinned").unwrap();
    assert_eq!(pinned.default_value.as_deref(), Some("0"));
    assert!(matches!(block_on(conn.describe_table(TableInfo::new("main", "nope"))), Err(Error::TableNotFound(_))));
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

/// Saves, conflicts and rollbacks against a scratch table `table` with rows (1 a) (2 b) (3 c).
fn exercise_edits(conn: &Connection, table: TableInfo) {
    let names = |conn: &Connection| -> Vec<String> {
        let page = block_on(conn.fetch_rows(table.clone(), 100, 0)).unwrap();
        column(&page, "name").iter().map(|v| v.display()).collect()
    };
    let columns = block_on(conn.fetch_rows(table.clone(), 1, 0)).unwrap().columns;
    let apply = |changes: Vec<RowChange>| block_on(conn.apply_changes(table.clone(), columns.clone(), changes));

    // Update + delete + insert in one transaction; booleans typed as text work everywhere.
    let affected = apply(vec![
        RowChange::Update { key: edit_key(1), set: vec![edit_set("name", edit_text("Ada")), edit_set("n", EditValue::Null), edit_set("active", edit_text("false"))] },
        RowChange::Delete { key: edit_key(2) },
        RowChange::Insert { values: vec![edit_set("id", EditValue::Default), edit_set("name", edit_text("Grace")), edit_set("n", edit_text("7"))] },
    ])
    .unwrap();
    assert_eq!(affected, 3);
    assert_eq!(names(conn), ["Ada", "c", "Grace"]);
    let page = block_on(conn.fetch_rows(table.clone(), 100, 0)).unwrap();
    assert_eq!(column(&page, "n")[0], &Value::Null);
    assert_eq!(column(&page, "n")[2].display(), "7");
    assert_eq!(column(&page, "note")[2].display(), "hi", "defaults fill omitted columns");
    assert!(matches!(column(&page, "active")[0], Value::Bool(false) | Value::Int(0)), "{:?}", column(&page, "active")[0]);

    // Writing the value a cell already has still counts as matching its row.
    assert_eq!(apply(vec![RowChange::Update { key: edit_key(3), set: vec![edit_set("name", edit_text("c"))] }]).unwrap(), 1);

    // A vanished row fails the whole batch: the other update is rolled back too.
    let err = apply(vec![
        RowChange::Update { key: edit_key(3), set: vec![edit_set("name", edit_text("changed"))] },
        RowChange::Update { key: edit_key(2), set: vec![edit_set("name", edit_text("ghost"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("No row matches") && m.contains("Nothing was saved")), "{err:?}");
    assert_eq!(names(conn), ["Ada", "c", "Grace"]);

    // Server errors (bad value, unique violation) name the row and roll everything back.
    let err = apply(vec![
        RowChange::Update { key: edit_key(3), set: vec![edit_set("name", edit_text("changed"))] },
        RowChange::Insert { values: vec![edit_set("name", edit_text("Ada"))] },
    ])
    .unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.starts_with("Couldn’t save a new row") && m.ends_with("Nothing was saved.")), "{err:?}");
    assert_eq!(names(conn), ["Ada", "c", "Grace"]);
}

#[test]
fn saves_row_edits_in_one_transaction() {
    let db = seeded();
    let conn = open(&db);
    block_on(conn.execute(
        "create table edit_test (id integer primary key, name text not null unique, n integer,
           active boolean not null default 1, note text default 'hi');
         insert into edit_test (id, name, n) values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3);".into(),
    ))
    .unwrap();
    exercise_edits(&conn, TableInfo::new("main", "edit_test"));
}

#[test]
fn script_results_know_their_tables_and_save_edits() {
    use dbcore::results::RowEdit;
    let file = seeded();
    let db = open(&file);
    block_on(db.execute("create table res_users (id integer primary key, name text); create table res_orders (id integer primary key, user_id integer references res_users (id), total numeric); insert into res_users values (1, 'Ada'); insert into res_orders values (10, 1, 5.5)".into())).unwrap();
    let result = block_on(db.execute(
        "select o.id, o.total, u.id as uid, u.name, o.total * 2 as twice from res_orders o join res_users u on u.id = o.user_id".into(),
    ))
    .unwrap();
    let origins: Vec<_> = result.origins.iter().map(|o| o.as_ref().map(|o| (o.schema.as_str(), o.table.as_str(), o.column.as_str()))).collect();
    assert_eq!(
        origins,
        [
            Some(("main", "res_orders", "id")),
            Some(("main", "res_orders", "total")),
            Some(("main", "res_users", "id")),
            Some(("main", "res_users", "name")),
            None,
        ]
    );
    let sources = block_on(db.describe_result(result.origins.clone())).unwrap();
    assert_eq!((sources.read_only_reason(1), sources.read_only_reason(3)), (None, None));
    assert!(sources.read_only_reason(4).is_some());
    let links: Vec<_> = sources.referenced_by.iter().map(|r| (r.table.as_str(), r.values.clone())).collect();
    assert_eq!(links, [("res_orders", vec![2])]);

    let edit = RowEdit {
        values: result.rows[0].clone(),
        set: vec![(1, EditValue::Text("7".into())), (3, EditValue::Text("Grace".into()))],
        delete: false,
    };
    let changes = sources.changes(&[edit]).unwrap();
    assert_eq!(block_on(db.apply_result_changes(changes)).unwrap(), 2);
    let after = block_on(db.execute("select u.name from res_users u".into())).unwrap();
    assert_eq!(after.rows[0][0], Value::Text("Grace".into()));
    block_on(db.execute("drop table res_orders; drop table res_users".into())).unwrap();
}
