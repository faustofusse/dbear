use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use dbcore::dump::{CancelToken, Compression, DataStyle, DumpContent, DumpOptions, DumpProgress, DumpScope, ProgressFn};
use dbcore::restore::{RestoreOptions, RestoreProgress};
use dbcore::{DatabaseKind, Schema, TableInfo};
use dbservice::{
    civil_date, default_restore_options, dump_options, dump_scope, normalize_dump_options, restore_options, today, DumpChoices, RunOptions, Target,
};

use crate::{block_on, Fixture};

#[test]
fn dump_options_keep_only_what_applies() {
    let all = DumpChoices { content: DumpContent::SchemaAndData, compression: Compression::Gzip, data_style: DataStyle::Copy, drop_objects: true, create_database: true };
    let pg = dump_options(DatabaseKind::Postgres, all, DumpScope::Database);
    assert_eq!((pg.data_style, pg.drop_objects, pg.create_database, pg.compression), (DataStyle::Copy, true, false, Compression::Gzip));
    let my = dump_options(DatabaseKind::Mysql, all, DumpScope::Database);
    assert_eq!((my.data_style, my.drop_objects, my.create_database), (DataStyle::Insert, true, true));
    let data_only = DumpChoices { content: DumpContent::DataOnly, ..all };
    let my = dump_options(DatabaseKind::Mysql, data_only, DumpScope::Database);
    assert_eq!((my.drop_objects, my.create_database), (false, false));
    let lite = dump_options(DatabaseKind::Sqlite, all, DumpScope::Schemas(vec!["main".into()]));
    assert_eq!((lite.data_style, lite.create_database, lite.scope), (DataStyle::Insert, false, DumpScope::Schemas(vec!["main".into()])));
    // Idempotent, so options already built can go through it again.
    assert_eq!(normalize_dump_options(DatabaseKind::Mysql, my.clone()), my);
    assert_eq!(DumpChoices::default(), serde_json::from_str("{}").unwrap());
    assert_eq!(DumpChoices::default().data_style, DataStyle::Copy);
}

#[test]
fn restore_options_per_kind() {
    assert_eq!(default_restore_options(DatabaseKind::Postgres), RestoreOptions { single_transaction: true, stop_on_error: true });
    assert_eq!(default_restore_options(DatabaseKind::Mysql), RestoreOptions { single_transaction: false, stop_on_error: true });
    assert_eq!(restore_options(DatabaseKind::Mysql, true, false), RestoreOptions { single_transaction: false, stop_on_error: false });
    assert_eq!(restore_options(DatabaseKind::Sqlite, true, false), RestoreOptions { single_transaction: true, stop_on_error: true });
    assert_eq!(restore_options(DatabaseKind::Sqlite, false, false), RestoreOptions { single_transaction: false, stop_on_error: false });
}

#[test]
fn dump_scope_from_checked_tables() {
    let schema = |name: &str, tables: &[&str]| Schema { name: name.into(), tables: tables.iter().map(|t| TableInfo::new(name, *t)).collect() };
    let schemas = vec![schema("public", &["a", "b"]), schema("billing", &["c"])];
    let set = |items: &[(&str, &str)]| items.iter().map(|(s, t)| (s.to_string(), t.to_string())).collect::<BTreeSet<_>>();
    assert_eq!(dump_scope(&schemas, &set(&[]), true), DumpScope::Database);
    assert_eq!(dump_scope(&schemas, &set(&[("public", "a"), ("public", "b"), ("billing", "c")]), false), DumpScope::Database);
    assert_eq!(dump_scope(&schemas, &set(&[("billing", "c")]), false), DumpScope::Schemas(vec!["billing".into()]));
    assert_eq!(dump_scope(&schemas, &set(&[("public", "a"), ("billing", "c")]), false), DumpScope::Tables(vec![TableInfo::new("public", "a"), TableInfo::new("billing", "c")]));
    // An empty schema keeps "everything checked" from meaning the whole database.
    let with_empty = vec![schema("public", &["a"]), schema("empty", &[])];
    assert_eq!(dump_scope(&with_empty, &set(&[("public", "a")]), false), DumpScope::Schemas(vec!["public".into()]));
}

#[test]
fn dates_and_file_names() {
    assert_eq!(civil_date(0), "1970-01-01");
    assert_eq!(civil_date(11_016), "2000-02-29");
    assert_eq!(civil_date(20_240), "2025-06-01");
    assert_eq!(today().len(), 10);
    let fx = Fixture::new();
    let config = fx.sqlite("app", "shop.sqlite");
    let target = Target::default_for(&config);
    assert_eq!(fx.service.dump_file_name(&target, Compression::Gzip).unwrap(), format!("shop-{}.sql.gz", today()));
    let pg = fx.postgres("pg", "app", true);
    assert_eq!(fx.service.dump_file_name(&Target::new(pg.id, "reports"), Compression::None).unwrap(), format!("reports-{}.sql", today()));
}

#[test]
fn dumps_and_restores_through_targets() {
    let fx = Fixture::new();
    let source = Target::default_for(&fx.sqlite("source", "source.db"));
    let sql = "create table items (id integer primary key, name text); insert into items (name) values ('a'), ('b');";
    block_on(fx.service.run(&source, sql.into(), RunOptions::default())).unwrap().result.unwrap();

    let file = fx.path("dump.sql");
    let seen: Arc<Mutex<Vec<DumpProgress>>> = Arc::default();
    let progress: ProgressFn<DumpProgress> = {
        let seen = seen.clone();
        Arc::new(move |p: &DumpProgress| seen.lock().unwrap().push(p.clone()))
    };
    let options = DumpOptions { data_style: DataStyle::Copy, ..Default::default() };
    let summary = block_on(fx.service.dump(&source, file.clone(), options, progress, CancelToken::new())).unwrap();
    assert_eq!((summary.tables, summary.rows), (1, 2));
    assert!(std::fs::read_to_string(&file).unwrap().to_lowercase().contains("insert into"), "COPY became INSERTs for SQLite");

    let dest = Target::default_for(&fx.sqlite("dest", "dest.db"));
    assert!(block_on(fx.service.schemas(&dest, false)).unwrap().iter().all(|s| s.tables.is_empty()));
    assert!(fx.service.cached_schemas(&dest).is_some());
    let progress: ProgressFn<RestoreProgress> = Arc::new(|_| {});
    let restored = block_on(fx.service.restore(&dest, file, RestoreOptions::default(), progress, CancelToken::new())).unwrap();
    assert!(restored.statements >= 2);
    assert!(fx.service.cached_schemas(&dest).is_none(), "a restore drops the schema cache");
    let rows = block_on(fx.service.run(&dest, "select name from items order by id".into(), RunOptions::default())).unwrap();
    assert_eq!(rows.result.unwrap().rows.len(), 2);

    // Cancelled before it starts: nothing is written.
    let cancel = CancelToken::new();
    cancel.cancel();
    let file = fx.path("cancelled.sql");
    let result = block_on(fx.service.dump(&source, file.clone(), DumpOptions::default(), Arc::new(|_| {}), cancel));
    assert!(matches!(result, Err(dbcore::Error::Cancelled)), "{result:?}");
    assert!(!file.exists());
    assert!(block_on(fx.service.dump(&Target::new("missing", ""), fx.path("x.sql"), DumpOptions::default(), Arc::new(|_| {}), CancelToken::new())).is_err());
}
