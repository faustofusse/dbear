//! Integration tests against the dev Postgres (`scripts/dev-db.sh up`).
//! Skipped unless `DBEAR_TEST_POSTGRES=1`; `scripts/test-postgres.sh` sets it up.

use std::time::{Duration, Instant};

use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{mock, Connection, ConnectionConfig, Error, RowQuery, SortKey, SslMode, TableInfo, TableKind, Value};

fn enabled() -> bool {
    let on = std::env::var("DBEAR_TEST_POSTGRES").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set DBEAR_TEST_POSTGRES=1 (scripts/dev-db.sh up)");
    }
    on
}

fn dev_config() -> ConnectionConfig {
    mock::connections().into_iter().find(|c| c.id == mock::DEV_DATABASE).unwrap()
}

fn dev() -> Connection {
    Connection::new(dev_config())
}

/// Any executor works: the core runs on its own runtime.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn column<'a>(result: &'a dbcore::QueryResult, name: &str) -> Vec<&'a Value> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| &r[i]).collect()
}

#[test]
fn lists_columns_of_every_table() {
    if !enabled() {
        return;
    }
    let tables = block_on(dev().list_columns()).unwrap();
    let users = tables.iter().find(|t| t.schema == "public" && t.table == "users").expect("users");
    let names: Vec<_> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"id") && names.contains(&"email") && names.contains(&"name"), "{names:?}");
    let id = users.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.is_primary_key && !id.is_nullable);
}

#[test]
fn lists_databases_and_browses_another_one() {
    if !enabled() {
        return;
    }
    let dbs = block_on(dev().list_databases()).unwrap();
    assert!(dbs.contains(&"app_dev".to_string()) && dbs.contains(&"postgres".to_string()), "{dbs:?}");
    assert!(!dbs.iter().any(|d| d.starts_with("template")));
    // Same login, other database: a separate Connection.
    let other = Connection::new(dev_config().with_database("postgres"));
    let result = block_on(other.execute("select current_database()".into())).unwrap();
    assert_eq!(result.rows[0][0], Value::Text("postgres".into()));

    // No database configured: the server's `postgres` database.
    let unset = Connection::new(ConnectionConfig { database: String::new(), ..dev_config() });
    let result = block_on(unset.execute("select current_database()".into())).unwrap();
    assert_eq!(result.rows[0][0], Value::Text("postgres".into()));
    assert!(block_on(unset.list_databases()).unwrap().contains(&"app_dev".to_string()));
}

#[test]
fn lists_schemas_and_relations() {
    if !enabled() {
        return;
    }
    let schemas = block_on(dev().list_schemas()).unwrap();
    let names: Vec<_> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["analytics", "archive", "billing", "public"]);

    let analytics = &schemas[0];
    let tables: Vec<_> = analytics.tables.iter().map(|t| t.name.as_str()).collect();
    // Partitions are hidden behind their parent; materialized views show as views.
    assert_eq!(tables, ["daily_signups", "events", "raw_log"]);
    assert_eq!(analytics.tables[0].kind, TableKind::View);
    assert_eq!(analytics.tables[1].estimated_row_count, Some(50_000));

    assert!(schemas[1].tables.is_empty(), "archive is empty");

    let public = &schemas[3];
    let active = public.tables.iter().find(|t| t.name == "active_users").unwrap();
    assert_eq!((active.kind, active.estimated_row_count), (TableKind::View, None));
}

#[test]
fn fetches_pages_in_primary_key_order() {
    if !enabled() {
        return;
    }
    let users = TableInfo::new("public", "users");
    let first = block_on(dev().fetch_rows(users.clone(), 50, 0)).unwrap();
    assert_eq!(first.total_count, Some(248));
    let page = block_on(dev().fetch_rows(users.clone(), 50, 100)).unwrap();
    assert_eq!(page.rows.len(), 50);
    // Only the first page pays for count(*).
    assert_eq!(page.total_count, None);
    assert_eq!(page.rows[0][0], Value::Int(101));
    assert_eq!(page.rows[49][0], Value::Int(150));

    let id = &page.columns[0];
    assert!(id.is_primary_key && !id.is_nullable);
    assert_eq!(id.type_name, "bigint");
    let last_login = page.columns.iter().find(|c| c.name == "last_login_at").unwrap();
    assert!(last_login.is_nullable);
    assert_eq!(last_login.type_name, "timestamp with time zone");

    let tail = block_on(dev().fetch_rows(users, 50, 240)).unwrap();
    assert_eq!(tail.rows.len(), 8);
}

#[test]
fn decodes_column_types() {
    if !enabled() {
        return;
    }
    let users = block_on(dev().fetch_rows(TableInfo::new("public", "users"), 4, 0)).unwrap();
    assert!(matches!(column(&users, "is_admin")[0], Value::Bool(false)));
    assert_eq!(column(&users, "tags")[0], &Value::Text("{customer,vip}".into()));
    assert!(matches!(column(&users, "settings")[0], Value::Text(s) if s.starts_with('{') && s.contains("theme")));
    assert_eq!(column(&users, "last_login_at")[0], &Value::Null);

    let sessions = block_on(dev().fetch_rows(TableInfo::new("public", "sessions"), 1, 0)).unwrap();
    assert!(matches!(column(&sessions, "id")[0], Value::Text(s) if s.len() == 36));
    assert!(matches!(column(&sessions, "token")[0], Value::Text(s) if s.starts_with("\\x")));
    // Ordered by a random uuid, so check shapes rather than exact values.
    assert!(matches!(column(&sessions, "ip")[0], Value::Text(s) if s.starts_with("10.0.")));
    assert!(matches!(column(&sessions, "ttl")[0], Value::Text(s) if s.ends_with(":00:00") || s.contains("day")));

    let orders = block_on(dev().fetch_rows(TableInfo::new("public", "orders"), 1, 0)).unwrap();
    assert_eq!(column(&orders, "status")[0], &Value::Text("paid".into()));
    assert!(matches!(column(&orders, "total")[0], Value::Decimal(_)));

    // NUMERIC beyond f64 precision must survive untouched.
    let payments = block_on(dev().fetch_rows(TableInfo::new("billing", "payments"), 1, 0)).unwrap();
    assert!(matches!(column(&payments, "fx_rate")[0], Value::Decimal(s) if s.ends_with(".56789012345678901234")));
}

#[test]
fn pages_tables_without_primary_key_and_views() {
    if !enabled() {
        return;
    }
    let log = block_on(dev().fetch_rows(TableInfo::new("analytics", "raw_log"), 10, 290)).unwrap();
    assert_eq!(log.rows.len(), 10);
    assert_eq!(log.total_count, None); // not the first page
    assert_eq!(log.rows[0][1], Value::Int(1)); // smallint

    let view = block_on(dev().fetch_rows(TableInfo::new("public", "active_users"), 500, 0)).unwrap();
    assert_eq!(view.total_count, None);
    assert!(!view.rows.is_empty());

    let events = block_on(dev().fetch_rows(TableInfo::new("analytics", "events"), 5, 0)).unwrap();
    assert_eq!(events.rows.len(), 5);
    assert_eq!(events.total_count, Some(50_000));
}

#[test]
fn missing_table_is_reported() {
    if !enabled() {
        return;
    }
    let err = block_on(dev().fetch_rows(TableInfo::new("public", "nope"), 5, 0)).unwrap_err();
    assert_eq!(err, Error::TableNotFound("public.nope".into()));
}

#[test]
fn executes_queries_with_typed_columns() {
    if !enabled() {
        return;
    }
    let result = block_on(dev().execute("select id, total, now() as at from orders order by id limit 3".into())).unwrap();
    let types: Vec<_> = result.columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(types, ["int8", "numeric", "timestamptz"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[2][0], Value::Int(3));
}

#[test]
fn scripts_return_last_result_or_affected_rows() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let rows = block_on(conn.execute("select 1 as a; select 'x' as b, 2 as c;".into())).unwrap();
    assert_eq!(rows.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    // Multi-statement scripts can't be prepared, so values stay text.
    assert_eq!(rows.rows[0][1], Value::Text("2".into()));

    let affected = block_on(conn.execute(
        "create temp table t (x int); insert into t select generate_series(1, 5); update t set x = x + 1 where x > 2"
            .into(),
    ))
    .unwrap();
    assert!(affected.columns.is_empty());
    assert_eq!(affected.rows_affected, Some(3));

    // The temp table lives on the script session, so it's still there.
    let wrapped = block_on(conn.execute("begin; select count(*) from t; commit".into())).unwrap();
    assert_eq!(wrapped.rows[0][0], Value::Text("5".into()));
}

#[test]
fn caps_script_rows_but_runs_the_whole_script() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let r = block_on(conn.execute_limited("select g from generate_series(1, 25000) g".into(), Some(1000))).unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.total_count), (1000, true, Some(25_000)));
    assert_eq!(r.rows[999][0], Value::Int(1000));

    // Statements after the capped one still run (the stream is drained, not cancelled).
    let script = "create temp table capped (x int); \
                  insert into capped select generate_series(1, 50); \
                  select * from capped; \
                  insert into capped values (51)";
    let r = block_on(conn.execute_limited(script.into(), Some(10))).unwrap();
    assert_eq!(r.rows.len(), 10);
    let count = block_on(conn.execute("select count(*) from capped".into())).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(51));

    let small = block_on(conn.execute_limited("select 1".into(), Some(10))).unwrap();
    assert_eq!((small.truncated, small.total_count), (false, None));
}

#[test]
fn reports_errors_with_position() {
    if !enabled() {
        return;
    }
    let err = block_on(dev().execute("select 1\nfrom users\nwhere nope = 1".into())).unwrap_err();
    let Error::Query(message) = err else { panic!("{err:?}") };
    assert!(message.starts_with("ERROR: column \"nope\" does not exist (line 3, column 7)"), "{message}");
}

#[test]
fn cancels_running_query() {
    if !enabled() {
        return;
    }
    let conn = dev();
    block_on(conn.connect()).unwrap();
    let started = Instant::now();
    let (result, ()) = block_on(async {
        tokio::join!(conn.execute("select pg_sleep(10)".into()), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            conn.cancel().await;
        })
    });
    assert_eq!(result.unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn dropping_the_future_cancels_on_the_server() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let marker = "dbear_drop_test_marker";
    block_on(async {
        let sql = format!("select pg_sleep(20) as {marker}");
        let _ = tokio::time::timeout(Duration::from_millis(500), conn.execute(sql)).await;
    });

    // Watch from another connection until the backend is no longer running it.
    let observer = dev();
    let still_running = || {
        let sql = format!(
            "select count(*) from pg_stat_activity where state = 'active' and query like '%{marker}%' and pid <> pg_backend_pid()"
        );
        let r = block_on(observer.execute(sql)).unwrap();
        r.rows[0][0] != Value::Int(0)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while still_running() {
        assert!(Instant::now() < deadline, "query kept running after its future was dropped");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn reports_and_closes_connections() {
    if !enabled() {
        return;
    }
    let conn = dev();
    assert!(!block_on(conn.is_connected()));
    block_on(conn.list_schemas()).unwrap();
    assert!(block_on(conn.is_connected()));

    // Disconnecting also stops a running script.
    let started = Instant::now();
    let (result, ()) = block_on(async {
        tokio::join!(conn.execute("select pg_sleep(10)".into()), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            conn.disconnect().await;
        })
    });
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!block_on(conn.is_connected()));

    // And the next call reconnects.
    block_on(conn.list_schemas()).unwrap();
    assert!(block_on(conn.is_connected()));
}

#[test]
fn bad_credentials_and_unreachable_hosts_fail_to_connect() {
    if !enabled() {
        return;
    }
    let wrong_password = ConnectionConfig { password: Some("nope".into()), ..dev_config() };
    let err = block_on(Connection::new(wrong_password).connect()).unwrap_err();
    assert!(matches!(&err, Error::ConnectionFailed(m) if m.contains("password authentication failed")), "{err:?}");

    let closed_port = ConnectionConfig { port: Some(1), ..dev_config() };
    assert!(matches!(block_on(Connection::new(closed_port).connect()), Err(Error::ConnectionFailed(_))));

    // The dev server has no TLS: prefer falls back to plain, require must fail.
    let require = ConnectionConfig { ssl_mode: SslMode::Require, ..dev_config() };
    assert!(matches!(block_on(Connection::new(require).connect()), Err(Error::ConnectionFailed(_))));
}

fn sorted(column: &str, descending: bool) -> RowQuery {
    RowQuery { sort: vec![SortKey { column: column.into(), descending }], filter: None }
}

fn filtered(filter: &str) -> RowQuery {
    RowQuery { filter: Some(filter.into()), ..Default::default() }
}

#[test]
fn sorts_and_pages_by_any_column() {
    if !enabled() {
        return;
    }
    let users = TableInfo::new("public", "users");
    let first = block_on(dev().fetch_rows_with(users.clone(), sorted("email", true), 100, 0)).unwrap();
    let second = block_on(dev().fetch_rows_with(users.clone(), sorted("email", true), 100, 100)).unwrap();
    let emails: Vec<String> = column(&first, "email").into_iter().chain(column(&second, "email")).map(|v| v.display()).collect();
    assert_eq!(emails.len(), 200);
    assert!(emails.windows(2).all(|w| w[0] >= w[1]), "not descending");
    assert_eq!(first.total_count, Some(248));

    // Ties (many users share a name) are broken by the primary key, so pages never overlap.
    let mut ids: Vec<String> = Vec::new();
    for offset in (0..248).step_by(50) {
        let page = block_on(dev().fetch_rows_with(users.clone(), sorted("name", false), 50, offset)).unwrap();
        ids.extend(column(&page, "id").into_iter().map(|v| v.display()));
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 248);

    let err = block_on(dev().fetch_rows_with(users, sorted("nope", false), 10, 0)).unwrap_err();
    assert!(matches!(err, Error::Query(m) if m.contains("nope")));
}

#[test]
fn filters_rows_and_counts_matches() {
    if !enabled() {
        return;
    }
    let users = TableInfo::new("public", "users");
    let admins = block_on(dev().fetch_rows_with(users.clone(), filtered("where is_admin;"), 100, 0)).unwrap();
    assert!(column(&admins, "is_admin").iter().all(|v| **v == Value::Bool(true)));
    assert_eq!(admins.total_count, Some(admins.rows.len() as u64));
    assert_eq!(admins.rows.len(), 14);

    // Sort and filter together, with a trailing comment.
    let mut query = sorted("id", true);
    query.filter = Some("id <= 10 -- small ones".into());
    let page = block_on(dev().fetch_rows_with(users.clone(), query, 3, 0)).unwrap();
    assert_eq!(column(&page, "id"), [&Value::Int(10), &Value::Int(9), &Value::Int(8)]);
    assert_eq!(page.total_count, Some(10));

    // Errors read like the user's filter, without positions into the generated SQL.
    let err = block_on(dev().fetch_rows_with(users.clone(), filtered("nope = 1"), 10, 0)).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("nope") && !m.contains("line")), "{err:?}");
    let err = block_on(dev().fetch_rows_with(users.clone(), filtered("true; delete from users"), 10, 0)).unwrap_err();
    assert!(matches!(err, Error::Query(_)));
    assert_eq!(block_on(dev().fetch_rows(users, 1, 0)).unwrap().total_count, Some(248));

    // Views have no count; filtering still works.
    let view = block_on(dev().fetch_rows_with(TableInfo::new("public", "active_users"), filtered("false"), 10, 0)).unwrap();
    assert!(view.rows.is_empty());
}

#[test]
fn describes_tables_views_and_partitions() {
    if !enabled() {
        return;
    }
    let users = block_on(dev().describe_table(TableInfo::new("public", "users"))).unwrap();
    assert_eq!(users.primary_key, ["id"]);
    let id = users.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.default_value.as_deref(), Some("GENERATED ALWAYS AS IDENTITY"));
    let admin = users.columns.iter().find(|c| c.name == "is_admin").unwrap();
    assert_eq!((admin.default_value.as_deref(), admin.is_nullable), (Some("false"), false));
    assert!(users.indexes.iter().any(|i| i.is_primary && i.columns == ["id"]));
    assert!(users.indexes.iter().any(|i| i.is_unique && !i.is_primary && i.columns == ["email"]));
    let ddl = users.ddl.unwrap();
    assert!(ddl.starts_with("CREATE TABLE \"public\".\"users\" (\n    \"id\" bigint GENERATED ALWAYS AS IDENTITY NOT NULL,"), "{ddl}");
    assert!(ddl.contains("UNIQUE (email)") && ddl.contains("PRIMARY KEY (id)"), "{ddl}");
    // Indexes that back constraints aren't repeated as CREATE INDEX.
    assert!(!ddl.contains("CREATE UNIQUE INDEX"), "{ddl}");

    let orders = block_on(dev().describe_table(TableInfo::new("public", "orders"))).unwrap();
    let fk = orders.foreign_keys.iter().find(|f| f.columns == ["user_id"]).expect("orders.user_id fk");
    assert_eq!((fk.referenced_schema.as_str(), fk.referenced_table.as_str()), ("public", "users"));
    assert_eq!((fk.referenced_columns.as_slice(), fk.on_delete.as_str()), (&["id".to_string()][..], "NO ACTION"));

    let view = block_on(dev().describe_table(TableInfo::new("public", "active_users"))).unwrap();
    assert!(view.ddl.unwrap().starts_with("CREATE VIEW \"public\".\"active_users\" AS\n"));
    assert!(view.primary_key.is_empty() && view.indexes.is_empty());

    let events = block_on(dev().describe_table(TableInfo::new("analytics", "events"))).unwrap();
    assert_eq!(events.primary_key, ["id", "created_at"]);
    assert!(events.ddl.unwrap().contains(") PARTITION BY RANGE (created_at);"));

    let err = block_on(dev().describe_table(TableInfo::new("public", "nope"))).unwrap_err();
    assert!(matches!(err, Error::TableNotFound(_)));
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
    if !enabled() {
        return;
    }
    // In the `postgres` database: app_dev's tables are counted by other tests running in parallel.
    let conn = Connection::new(dev_config().with_database("postgres"));
    block_on(conn.execute(
        "drop table if exists public.dbear_edit_test;
         create table public.dbear_edit_test (
           id int generated by default as identity primary key, name text not null unique,
           n int, active boolean not null default true, note text default 'hi');
         insert into public.dbear_edit_test (id, name, n) overriding system value values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3);
         select setval(pg_get_serial_sequence('public.dbear_edit_test', 'id'), 3);".into(),
    ))
    .unwrap();
    exercise_edits(&conn, TableInfo::new("public", "dbear_edit_test"));

    // Postgres casts typed text to the column type, and reports bad input as the user's error.
    let table = TableInfo::new("public", "dbear_edit_test");
    let columns = block_on(conn.fetch_rows(table.clone(), 1, 0)).unwrap().columns;
    let err = block_on(conn.apply_changes(table, columns, vec![RowChange::Update { key: edit_key(1), set: vec![edit_set("n", edit_text("abc"))] }])).unwrap_err();
    assert!(matches!(&err, Error::Query(m) if m.contains("\"id\" = 1") && m.contains("integer")), "{err:?}");
    block_on(conn.execute("drop table public.dbear_edit_test".into())).unwrap();
}

#[test]
fn follows_a_foreign_key_to_its_row() {
    if !enabled() {
        return;
    }
    let orders = TableInfo::new("public", "orders");
    let fk = block_on(dev().describe_table(orders.clone()))
        .unwrap()
        .foreign_keys
        .into_iter()
        .find(|fk| fk.columns == ["user_id"])
        .expect("orders.user_id references users");
    assert_eq!((fk.referenced_table.as_str(), fk.referenced_columns.as_slice()), ("users", ["id".to_string()].as_slice()));

    let first = block_on(dev().fetch_rows(orders, 1, 0)).unwrap();
    let user_id = column(&first, "user_id")[0].clone();
    let dialect = dbcore::dialect::Dialect(dbcore::DatabaseKind::Postgres);
    let users = TableInfo::new(&fk.referenced_schema, &fk.referenced_table);
    let row = block_on(dev().fetch_rows_with(users.clone(), filtered(&dialect.match_filter(&fk.referenced_columns, std::slice::from_ref(&user_id))), 10, 0)).unwrap();
    assert_eq!(column(&row, "id"), [&user_id]);

    // Text values are cast by the database (here to text; uuid/date keys work the same way).
    let email = column(&row, "email")[0].clone();
    let by_email = block_on(dev().fetch_rows_with(users, filtered(&dialect.match_filter(&["email".into()], &[email])), 10, 0)).unwrap();
    assert_eq!(column(&by_email, "id"), [&user_id]);

    // And back: the tables pointing at users, then that user's orders.
    let structure = block_on(dev().describe_table(TableInfo::new("public", "users"))).unwrap();
    let incoming: Vec<(&str, &str)> = structure.referenced_by.iter().map(|k| (k.schema.as_str(), k.table.as_str())).collect();
    assert_eq!(incoming, [("billing", "subscriptions"), ("public", "orders"), ("public", "sessions")]);
    let key = structure.referenced_by.iter().find(|k| k.table == "orders").unwrap();
    assert_eq!((key.columns.as_slice(), key.referenced_columns.as_slice()), (&["user_id".to_string()][..], &["id".to_string()][..]));
    let filter = dialect.match_filter(&key.columns, std::slice::from_ref(&user_id));
    let their_orders = block_on(dev().fetch_rows_with(TableInfo::new("public", "orders"), filtered(&filter), 1000, 0)).unwrap();
    assert!(!their_orders.rows.is_empty());
    assert!(column(&their_orders, "user_id").iter().all(|v| **v == user_id));
}
