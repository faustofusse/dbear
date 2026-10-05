//! Integration tests against the dev MySQL (`scripts/dev-db.sh up mysql`).
//! Skipped unless `DBEAR_TEST_MYSQL=1`; `scripts/test-mysql.sh` sets it up.

use std::time::{Duration, Instant};

use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{mock, Connection, ConnectionConfig, Error, RowQuery, SortKey, SslMode, TableInfo, TableKind, Value};

fn enabled() -> bool {
    let on = std::env::var("DBEAR_TEST_MYSQL").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set DBEAR_TEST_MYSQL=1 (scripts/dev-db.sh up mysql)");
    }
    on
}

fn dev_config() -> ConnectionConfig {
    mock::connections().into_iter().find(|c| c.id == mock::DEV_MYSQL).unwrap()
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

#[test]
fn lists_columns_of_every_table() {
    if !enabled() {
        return;
    }
    let tables = block_on(dev().list_columns()).unwrap();
    let customers = tables.iter().find(|t| t.schema == "shop" && t.table == "customers").expect("customers");
    let names: Vec<_> = customers.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"id") && names.contains(&"email") && names.contains(&"name"), "{names:?}");
    let id = customers.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.is_primary_key && !id.is_nullable);
}

#[test]
fn lists_databases_as_schemas() {
    if !enabled() {
        return;
    }
    let schemas = block_on(dev().list_schemas()).unwrap();
    let names: Vec<_> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["archive", "blog", "shop"], "system schemas are hidden, empty ones kept");
    let shop = schemas.iter().find(|s| s.name == "shop").unwrap();
    let tables: Vec<_> = shop.tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
    assert_eq!(
        tables,
        [
            ("audit_log", TableKind::Table),
            ("customers", TableKind::Table),
            ("events", TableKind::Table),
            ("order_items", TableKind::Table),
            ("orders", TableKind::Table),
            ("paid_orders", TableKind::View),
            ("products", TableKind::Table),
        ]
    );
    assert!(shop.tables.iter().find(|t| t.name == "events").unwrap().estimated_row_count.unwrap() > 10_000);

    // With "show all databases" off, only the configured database is listed.
    let only_blog = ConnectionConfig { database: "blog".into(), show_all_databases: false, ..dev_config() };
    let schemas = block_on(Connection::new(only_blog).list_schemas()).unwrap();
    assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["blog"]);
}

#[test]
fn pages_tables_with_typed_values() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let first = block_on(conn.fetch_rows(TableInfo::new("shop", "customers"), 100, 0)).unwrap();
    assert_eq!((first.rows.len(), first.total_count), (100, Some(250)));
    let type_of = |name: &str| first.columns.iter().find(|c| c.name == name).unwrap().type_name.clone();
    assert_eq!((type_of("id").as_str(), type_of("balance").as_str()), ("int unsigned", "decimal(12,2)"));
    assert!(first.columns[0].is_primary_key && !first.columns[0].is_nullable);

    assert_eq!(column(&first, "id")[..3], [&Value::Int(1), &Value::Int(2), &Value::Int(3)]);
    assert_eq!(column(&first, "is_active")[6], &Value::Bool(false)); // id 7
    assert_eq!(column(&first, "balance")[0], &Value::Decimal("37.13".into()));
    assert_eq!(column(&first, "rating")[4], &Value::Null); // id 5
    assert_eq!(column(&first, "rating")[0], &Value::Float(0.1));
    assert_eq!(column(&first, "tags")[0], &Value::Text("beta,wholesale".into()));
    assert!(matches!(column(&first, "avatar")[2], Value::Text(t) if t.starts_with("0x") && t.len() == 34));
    assert_eq!(column(&first, "created_at")[0], &Value::Text("2025-01-01 01:00:00.000".into()));

    let last = block_on(conn.fetch_rows(TableInfo::new("shop", "customers"), 100, 200)).unwrap();
    assert_eq!((last.rows.len(), last.total_count), (50, None));
    assert_eq!(column(&last, "id")[0], &Value::Int(201));
}

#[test]
fn decodes_mysql_specific_types() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let products = block_on(conn.fetch_rows(TableInfo::new("shop", "products"), 5, 0)).unwrap();
    assert_eq!(column(&products, "flags")[2], &Value::Int(3)); // bit(8)
    assert_eq!(column(&products, "released")[0], &Value::Int(2016)); // year
    assert_eq!(column(&products, "attributes")[0], &Value::Text(r#"{"color": "white", "wireless": false}"#.into()));

    let events = block_on(conn.fetch_rows(TableInfo::new("shop", "events"), 2, 0)).unwrap();
    // bigint unsigned above i64::MAX stays exact.
    assert_eq!(column(&events, "big_counter")[0], &Value::Decimal("18446744073709551614".into()));

    let orders = block_on(conn.fetch_rows(TableInfo::new("shop", "orders"), 2, 0)).unwrap();
    assert_eq!(column(&orders, "fx_rate")[0], &Value::Decimal("2.24691357802469135780".into()));
    assert_eq!(column(&orders, "status")[0], &Value::Text("paid".into()));
}

#[test]
fn pages_composite_and_keyless_tables_and_views() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let items = block_on(conn.fetch_rows(TableInfo::new("shop", "order_items"), 3, 0)).unwrap();
    assert_eq!(column(&items, "order_id"), [&Value::Int(1), &Value::Int(1), &Value::Int(1)]);
    let log = block_on(conn.fetch_rows(TableInfo::new("shop", "audit_log"), 100, 0)).unwrap();
    assert_eq!((log.rows.len(), log.total_count), (40, Some(40)));
    let view = block_on(conn.fetch_rows(TableInfo::new("shop", "paid_orders"), 1000, 0)).unwrap();
    assert_eq!((view.rows.len(), view.total_count), (300, None));
    let err = block_on(conn.fetch_rows(TableInfo::new("shop", "nope"), 10, 0)).unwrap_err();
    assert_eq!(err, Error::TableNotFound("shop.nope".into()));
}

#[test]
fn runs_scripts_across_databases() {
    if !enabled() {
        return;
    }
    let conn = dev();
    let r = block_on(conn.execute(
        "select count(*) as n from shop.orders; select title, published from blog.posts order by id limit 3".into(),
    ))
    .unwrap();
    assert_eq!(r.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(), ["varchar", "tinyint(1)"]);
    assert_eq!(column(&r, "published"), [&Value::Bool(true), &Value::Bool(true), &Value::Bool(false)]);

    let r = block_on(conn.execute(
        "create temporary table shop.tmp (x int); insert into shop.tmp values (1), (2), (3)".into(),
    ))
    .unwrap();
    assert_eq!((r.rows_affected, r.columns.len()), (Some(3), 0));

    let r = block_on(conn.execute_limited("select * from shop.events".into(), Some(1000))).unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.total_count), (1000, true, Some(50_000)));
    // The session is clean afterwards.
    assert_eq!(block_on(conn.execute("select 1 as one".into())).unwrap().rows, [[Value::Int(1)]]);
}

#[test]
fn reports_server_errors() {
    if !enabled() {
        return;
    }
    let err = block_on(dev().execute("select 1;\nselec nope".into())).unwrap_err();
    let Error::Query(message) = err else { panic!("{err:?}") };
    assert!(message.starts_with("ERROR 1064 (42000)") && message.contains("line 1"), "{message}");

    let err = block_on(dev().execute("select * from shop.nope".into())).unwrap_err();
    assert_eq!(err, Error::Query("ERROR 1146 (42S02): Table 'shop.nope' doesn't exist".into()));
}

#[test]
fn cancels_long_scripts() {
    if !enabled() {
        return;
    }
    let conn = dev();
    block_on(conn.connect()).unwrap();
    let started = Instant::now();
    let runner = conn.clone();
    let handle = std::thread::spawn(move || block_on(runner.execute("select sleep(30)".into())));
    std::thread::sleep(Duration::from_millis(500));
    block_on(conn.cancel());
    assert_eq!(handle.join().unwrap().unwrap_err(), Error::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(block_on(conn.execute("select 2".into())).unwrap().rows, [[Value::Int(2)]]);
}

#[test]
fn connects_with_each_ssl_mode_and_reports_auth_errors() {
    if !enabled() {
        return;
    }
    for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require] {
        let config = ConnectionConfig { ssl_mode: mode, ..dev_config() };
        block_on(Connection::new(config).connect()).unwrap_or_else(|e| panic!("{mode:?}: {e}"));
    }
    let wrong = ConnectionConfig { password: Some("wrong".into()), ..dev_config() };
    let err = block_on(Connection::new(wrong).connect()).unwrap_err();
    assert!(matches!(err, Error::ConnectionFailed(ref m) if m.contains("Access denied")), "{err:?}");
}

#[test]
fn reports_and_closes_connections() {
    if !enabled() {
        return;
    }
    let conn = dev();
    assert!(!block_on(conn.is_connected()));
    block_on(conn.connect()).unwrap();
    assert!(block_on(conn.is_connected()));
    block_on(conn.disconnect());
    assert!(!block_on(conn.is_connected()));
    // Reconnects lazily.
    assert!(!block_on(conn.list_schemas()).unwrap().is_empty());
}

#[test]
fn sorts_filters_and_rejects_smuggled_statements() {
    if !enabled() {
        return;
    }
    let customers = TableInfo::new("shop", "customers");
    let query = RowQuery { sort: vec![SortKey { column: "balance".into(), descending: true }], filter: Some("is_active".into()) };
    let page = block_on(dev().fetch_rows_with(customers.clone(), query, 500, 0)).unwrap();
    assert!(!page.rows.is_empty());
    let balances: Vec<f64> = column(&page, "balance").iter().map(|v| v.display().parse().unwrap()).collect();
    assert!(balances.windows(2).all(|w| w[0] >= w[1]));
    assert!(column(&page, "is_active").iter().all(|v| **v == Value::Bool(true)));
    assert_eq!(page.total_count, Some(page.rows.len() as u64));

    // The text protocol would run a second statement: the core refuses it before it gets there.
    for evil in [r"name = '\'' ; drop table shop.orders; '", "1 = 1 # '\n; drop table shop.orders; -- '", "1--1; drop table shop.orders"] {
        let filter = RowQuery { filter: Some(evil.into()), ..Default::default() };
        let err = block_on(dev().fetch_rows_with(customers.clone(), filter, 1, 0)).unwrap_err();
        assert!(matches!(err, Error::Query(_)), "{evil}: {err:?}");
    }
    assert!(block_on(dev().fetch_rows(TableInfo::new("shop", "orders"), 1, 0)).is_ok());
}

#[test]
fn describes_tables_and_views() {
    if !enabled() {
        return;
    }
    let orders = block_on(dev().describe_table(TableInfo::new("shop", "orders"))).unwrap();
    assert_eq!(orders.primary_key, ["id"]);
    let id = orders.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.default_value.as_deref(), Some("auto_increment"));
    let status = orders.columns.iter().find(|c| c.name == "status").unwrap();
    assert_eq!(status.default_value.as_deref(), Some("pending"));
    let fk = &orders.foreign_keys[0];
    assert_eq!((fk.columns.as_slice(), fk.referenced_table.as_str()), (&["customer_id".to_string()][..], "customers"));
    assert!(orders.indexes.iter().any(|i| i.is_primary && i.columns == ["id"]));
    assert!(orders.ddl.unwrap().starts_with("CREATE TABLE `orders`"));

    let customers = block_on(dev().describe_table(TableInfo::new("shop", "customers"))).unwrap();
    let from_orders = customers.referenced_by.iter().find(|k| k.table == "orders").expect("orders.customer_id references customers");
    assert_eq!((from_orders.schema.as_str(), from_orders.columns.as_slice()), ("shop", &["customer_id".to_string()][..]));
    assert_eq!(from_orders.referenced_columns, ["id"]);

    let items = block_on(dev().describe_table(TableInfo::new("shop", "order_items"))).unwrap();
    assert_eq!(items.primary_key, ["order_id", "product_id"]);

    let view = block_on(dev().describe_table(TableInfo::new("shop", "paid_orders"))).unwrap();
    assert!(view.ddl.unwrap().contains("VIEW `shop`.`paid_orders` AS"));
    assert!(matches!(block_on(dev().describe_table(TableInfo::new("shop", "nope"))), Err(Error::TableNotFound(_))));
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
    let conn = dev();
    block_on(conn.execute(
        "drop table if exists archive.dbear_edit_test;
         create table archive.dbear_edit_test (
           id int auto_increment primary key, name varchar(50) not null unique,
           n int, active boolean not null default true, note varchar(20) default 'hi');
         insert into archive.dbear_edit_test (id, name, n) values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3);".into(),
    ))
    .unwrap();
    exercise_edits(&conn, TableInfo::new("archive", "dbear_edit_test"));
    block_on(conn.execute("drop table archive.dbear_edit_test".into())).unwrap();
}

#[test]
fn browses_one_database_and_lists_the_others() {
    if !enabled() {
        return;
    }
    let shop = Connection::new(dev_config().with_database("shop"));
    let schemas = block_on(shop.list_schemas()).unwrap();
    assert_eq!(schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["shop"]);
    assert_eq!(block_on(shop.list_databases()).unwrap(), ["archive", "blog", "shop"]);
    let columns = block_on(shop.list_columns()).unwrap();
    assert!(columns.iter().all(|t| t.schema == "shop"), "completion only sees this database");
    // The database is the session's default: unqualified names work in scripts.
    let count = block_on(shop.execute("select count(*) as n from customers".into())).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(250));
    assert!(dev_config().supports_multiple_databases());
}

#[test]
fn creates_grants_and_drops_an_account() {
    use dbcore::access::{AccessChange, GrantObject, PrivilegeSet, RoleRef, RoleSpec};
    if !enabled() {
        return;
    }
    let conn = dev();
    let me = RoleRef::new("dbear_test_access", Some("%".into()));
    let reader = RoleRef::new("dbear_test_reader", Some("%".into()));
    block_on(conn.execute("drop user if exists 'dbear_test_access'@'%', 'dbear_test_reader'@'%'".into())).unwrap();
    let group = RoleSpec { name: reader.name.clone(), can_login: false, ..Default::default() };
    block_on(conn.apply_access(AccessChange::CreateRole(group))).unwrap();
    let spec = RoleSpec {
        name: me.name.clone(),
        host: me.host.clone(),
        password: Some("s3cret".into()),
        can_login: true,
        connection_limit: Some(7),
        member_of: vec![reader.clone()],
        ..Default::default()
    };
    block_on(conn.apply_access(AccessChange::CreateRole(spec))).unwrap();
    let roles = block_on(conn.list_roles()).unwrap();
    let role = roles.iter().find(|r| r.reference() == me).expect("created").clone();
    assert!(role.can_login);
    assert_eq!(role.connection_limit, Some(7));
    assert_eq!(role.member_of, vec![reader.clone()]);
    assert!(!roles.iter().find(|r| r.reference() == reader).unwrap().can_login);
    assert!(roles.iter().any(|r| r.is_system));

    let set = |p: &[&str], g: bool| PrivilegeSet { privileges: p.iter().map(|s| s.to_string()).collect(), grantable: g };
    let change = |object: GrantObject, before, after| {
        block_on(conn.apply_access(AccessChange::SetPrivileges { role: me.clone(), object, before, after })).unwrap()
    };
    change(GrantObject::Database { name: "shop".into() }, set(&[], false), set(&["SELECT", "INSERT"], true));
    change(GrantObject::Server, set(&[], false), set(&["PROCESS"], false));
    let grouped = dbcore::access::group_grants(&block_on(conn.list_grants(me.clone())).unwrap());
    assert!(grouped.contains(&(GrantObject::Server, set(&["PROCESS"], false))), "{grouped:?}");
    assert!(grouped.contains(&(GrantObject::Database { name: "shop".into() }, set(&["INSERT", "SELECT"], true))), "{grouped:?}");
    change(GrantObject::Database { name: "shop".into() }, set(&["INSERT", "SELECT"], true), set(&["SELECT"], false));
    let grouped = dbcore::access::group_grants(&block_on(conn.list_grants(me.clone())).unwrap());
    assert!(grouped.contains(&(GrantObject::Database { name: "shop".into() }, set(&["SELECT"], false))), "{grouped:?}");

    block_on(conn.apply_access(AccessChange::DropRole(me.clone()))).unwrap();
    block_on(conn.apply_access(AccessChange::DropRole(reader))).unwrap();
    assert!(!block_on(conn.list_roles()).unwrap().iter().any(|r| r.reference() == me));
}
