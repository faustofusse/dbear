use std::future::Future;
use std::sync::Arc;

use dbcore::complete::CompletionKind;
use dbcore::{TableInfo, Value};
use dbservice::{RunOptions, Target};

use crate::{block_on, Fixture};

fn tables(schemas: &[dbcore::Schema]) -> Vec<String> {
    let mut names: Vec<String> = schemas.iter().flat_map(|s| s.tables.iter().map(|t| t.name.clone())).collect();
    names.sort();
    names
}

/// A saved SQLite connection with a `users` table, and its target.
fn with_users(fx: &Fixture) -> Target {
    let config = fx.sqlite("app", "app.db");
    let target = Target::default_for(&config);
    let outcome = block_on(fx.service.run(&target, "create table users (id integer primary key, name text)".into(), RunOptions::default())).unwrap();
    outcome.result.unwrap();
    target
}

#[test]
fn caches_one_connection_per_target() {
    let fx = Fixture::new();
    let config = fx.sqlite("app", "app.db");
    let target = Target::default_for(&config);
    assert!(fx.service.cached(&target).is_none());
    let a = block_on(fx.service.connection(&target)).unwrap();
    let b = block_on(fx.service.connection(&target)).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let own = block_on(fx.service.new_connection(&target)).unwrap();
    assert!(!Arc::ptr_eq(&a, &own), "new_connection is never the shared one");
    assert_eq!(fx.service.open_connections().len(), 1);

    let dropped = fx.service.forget(&config.id);
    assert_eq!(dropped.len(), 1);
    assert!(fx.service.cached(&target).is_none());
    let c = block_on(fx.service.connection(&target)).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));

    assert!(block_on(fx.service.connection(&Target::new("missing", ""))).is_err());
    assert_eq!(block_on(fx.service.disconnect_all()), vec![config.id.clone()]);
    assert!(fx.service.open_connections().is_empty());
}

#[test]
fn load_caches_schemas_until_ddl() {
    let fx = Fixture::new();
    let target = with_users(&fx);
    let loaded = block_on(fx.service.load(&target, false)).unwrap();
    assert!(!loaded.cached);
    assert_eq!(tables(&loaded.schemas), ["users"]);
    assert_eq!(loaded.databases, None, "SQLite doesn't browse databases");
    assert!(fx.service.cached(&target).is_some());
    let again = block_on(fx.service.load(&target, false)).unwrap();
    assert!(again.cached);
    assert_eq!(fx.service.table_info(&target, "main", "users").name, "users");
    assert_eq!(fx.service.table_info(&target, "main", "nope"), TableInfo::new("main", "nope"));

    // DDL drops the cache; other statements don't.
    let insert = block_on(fx.service.run(&target, "insert into users (name) values ('ana')".into(), RunOptions::default())).unwrap();
    assert!(!insert.schema_changed);
    assert!(fx.service.cached_schemas(&target).is_some());
    let ddl = block_on(fx.service.run(&target, "create table orders (id integer primary key)".into(), RunOptions::default())).unwrap();
    assert!(ddl.schema_changed);
    assert!(fx.service.cached_schemas(&target).is_none());
    assert_eq!(tables(&block_on(fx.service.schemas(&target, false)).unwrap()), ["orders", "users"]);

    // Another process adds a table: only a refresh sees it.
    let other = dbcore::Connection::new(crate::sqlite_config("other", &fx.path("app.db")));
    block_on(other.execute("create table later (x)".into())).unwrap();
    assert!(block_on(fx.service.load(&target, false)).unwrap().cached);
    assert_eq!(tables(&block_on(fx.service.load(&target, true)).unwrap().schemas), ["later", "orders", "users"]);
}

#[test]
fn a_failed_load_keeps_no_connection() {
    let fx = Fixture::new();
    let config = fx.sqlite("broken", "missing/dir/app.db");
    let target = Target::default_for(&config);
    assert!(block_on(fx.service.load(&target, false)).is_err());
    assert!(fx.service.cached(&target).is_none());
    assert!(fx.service.cached_schemas(&target).is_none());
}

#[test]
fn forgetting_during_a_load_doesnt_cache_it_again() {
    let fx = Fixture::new();
    let target = with_users(&fx);
    fx.service.forget(&target.connection_id);
    let service = fx.service.clone();
    let load = service.load(&target, false);
    // Forgotten after the load captured its generation, before it finishes.
    let mut load = Box::pin(load);
    let waker = std::task::Waker::noop();
    let _ = load.as_mut().poll(&mut std::task::Context::from_waker(waker));
    fx.service.forget(&target.connection_id);
    let loaded = block_on(load).unwrap();
    assert_eq!(tables(&loaded.schemas), ["users"]);
    assert!(fx.service.cached(&target).is_none());
    assert!(fx.service.cached_schemas(&target).is_none());
}

#[test]
fn browsing_connections_list_their_databases_once() {
    let fx = Fixture::with(|o| o.samples = true);
    // A mock sample (no network): Postgres that shows all databases.
    let config = fx.service.config("staging-pg").unwrap();
    let target = fx.service.target_for(&config, None);
    assert_eq!(target, Target::new("staging-pg", "app"));
    let first = block_on(fx.service.load(&target, false)).unwrap();
    assert_eq!(first.databases.as_deref(), Some(&["app".to_string(), "app_test".into(), "postgres".into()][..]));
    assert!(!first.database_gone);
    assert_eq!(fx.service.databases("staging-pg").unwrap().len(), 3);
    assert!(!first.schemas.is_empty());

    // Another database of the same connection: its own connection, the list isn't fetched again.
    let other = Target::new("staging-pg", "app_test");
    let second = block_on(fx.service.load(&other, false)).unwrap();
    assert_eq!(second.databases, None);
    assert!(!Arc::ptr_eq(&fx.service.cached(&target).unwrap(), &fx.service.cached(&other).unwrap()));
    assert_eq!(fx.service.cached(&other).unwrap().config().database, "app_test");
    assert!(block_on(fx.service.load(&other, true)).unwrap().databases.is_some(), "refresh lists them again");
    fx.service.invalidate_databases("staging-pg");
    assert_eq!(fx.service.databases("staging-pg"), None);

    // The sample that can't connect.
    let unreachable = Target::new("prod-replica", "app");
    assert!(matches!(block_on(fx.service.load(&unreachable, false)), Err(dbcore::Error::ConnectionFailed(_))));
    assert!(fx.service.cached(&unreachable).is_none());

    // Status light: open connections the server still has.
    assert!(block_on(fx.service.lost_connections()).is_empty());
    block_on(fx.service.cached(&target).unwrap().disconnect());
    // `app_test` is still connected, so the connection isn't lost.
    assert!(block_on(fx.service.lost_connections()).is_empty());
    block_on(fx.service.cached(&other).unwrap().disconnect());
    assert_eq!(block_on(fx.service.lost_connections()).into_iter().collect::<Vec<_>>(), ["staging-pg"]);
    block_on(fx.service.disconnect(&other));
    assert!(fx.service.cached(&other).is_none() && fx.service.cached(&target).is_some());
}

#[test]
fn completes_from_a_cached_catalog() {
    let fx = Fixture::new();
    let target = with_users(&fx);
    let sql = "select * from us";
    let found = block_on(fx.service.complete(&target, sql, sql.len())).unwrap();
    assert!(found.items.iter().any(|i| i.label == "users" && i.kind == CompletionKind::Table), "{:?}", found.items);
    let catalog = fx.service.cached_catalog(&target).unwrap();
    assert!(Arc::ptr_eq(&catalog, &block_on(fx.service.catalog(&target)).unwrap()));

    // Columns too.
    let sql = "select na from users";
    let found = block_on(fx.service.complete(&target, sql, "select na".len())).unwrap();
    assert!(found.items.iter().any(|i| i.label == "name" && i.kind == CompletionKind::Column), "{:?}", found.items);

    // DDL drops the catalog; the next completion sees the new table.
    block_on(fx.service.run(&target, "create table usage_log (id)".into(), RunOptions::default())).unwrap();
    assert!(fx.service.cached_catalog(&target).is_none());
    let sql = "select * from usa";
    let found = block_on(fx.service.complete(&target, sql, sql.len())).unwrap();
    assert!(found.items.iter().any(|i| i.label == "usage_log"), "{:?}", found.items);

    // An offset past the end or inside a character is clamped.
    block_on(fx.service.complete(&target, "select é", 8)).unwrap();
    block_on(fx.service.complete(&target, "select", 99)).unwrap();
}

#[test]
fn keywords_complete_without_a_catalog() {
    let fx = Fixture::new();
    let config = fx.sqlite("broken", "missing/dir/app.db");
    let found = block_on(fx.service.complete(&Target::default_for(&config), "sel", 3)).unwrap();
    assert!(found.items.iter().any(|i| i.kind == CompletionKind::Keyword && i.label.eq_ignore_ascii_case("select")));
    assert!(fx.service.cached_catalog(&Target::default_for(&config)).is_none());
}

#[test]
fn related_rows_filter_finds_sqlites_implicit_key() {
    let fx = Fixture::new();
    let target = with_users(&fx);
    let connection = block_on(fx.service.connection(&target)).unwrap();
    let users = TableInfo::new("main", "users");
    let named = block_on(fx.service.related_filter(&connection, &users, &["name".into()], &[Value::Text("ana".into())])).unwrap();
    assert!(named.contains("name") && named.contains("'ana'"), "{named}");
    let implicit = block_on(fx.service.related_filter(&connection, &users, &[], &[Value::Int(7)])).unwrap();
    assert!(implicit.contains("id") && implicit.contains('7'), "{implicit}");
    assert!(block_on(fx.service.related_filter(&connection, &users, &[], &[Value::Int(1), Value::Int(2)])).is_err());
}

#[test]
fn connected_caches_only_what_connects() {
    let fx = Fixture::new();
    let broken = Target::default_for(&fx.sqlite("broken", "missing/dir/app.db"));
    assert!(block_on(fx.service.connected(&broken)).is_err());
    assert!(fx.service.cached(&broken).is_none(), "a failed connect leaves nothing cached");

    let target = Target::default_for(&fx.sqlite("app", "app.db"));
    let a = block_on(fx.service.connected(&target)).unwrap();
    assert!(Arc::ptr_eq(&a, &fx.service.cached(&target).unwrap()));
    assert!(Arc::ptr_eq(&a, &block_on(fx.service.connected(&target)).unwrap()));
}

#[test]
fn schema_targets_lists_what_was_loaded() {
    let fx = Fixture::new();
    assert!(fx.service.schema_targets().is_empty());
    let target = with_users(&fx);
    block_on(fx.service.load(&target, false)).unwrap();
    assert_eq!(fx.service.schema_targets(), std::slice::from_ref(&target));
    fx.service.schema_changed(&target);
    assert!(fx.service.schema_targets().is_empty());
}
