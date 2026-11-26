use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::{Error, TableInfo, TableKind, Value};
use dbservice::{RunOptions, TableEditing, Target};

use crate::{block_on, Fixture};

fn setup(fx: &Fixture) -> Target {
    let config = fx.sqlite("app", "app.db");
    let target = Target::default_for(&config);
    let sql = "create table items (id integer primary key, name text not null, data blob);
               insert into items (name) values ('a'), ('b');
               create table tags (item_id integer references items(id), tag text, primary key (item_id, tag));
               create view v as select * from items;";
    block_on(fx.service.run(&target, sql.into(), RunOptions { record_history: false, ..Default::default() })).unwrap().result.unwrap();
    target
}

fn key(id: i64) -> Vec<KeyValue> {
    vec![KeyValue { column: "id".into(), value: Value::Int(id) }]
}

fn set(column: &str, value: EditValue) -> CellEdit {
    CellEdit { column: column.into(), value }
}

fn editing(fx: &Fixture, target: &Target, table: TableInfo) -> TableEditing {
    block_on(fx.service.table_editing(target, &table)).unwrap().0
}

#[test]
fn describes_what_can_be_edited() {
    let fx = Fixture::new();
    let target = setup(&fx);
    let items = editing(&fx, &target, TableInfo::new("main", "items"));
    assert_eq!(items.read_only, None);
    assert_eq!(items.key, ["id"]);
    let data = &items.columns[2];
    assert!(data.binary && data.read_only.is_some());
    assert_eq!(items.referenced_by[0].table, "tags");
    assert_eq!(items.referenced_by[0].values, ["id"]);

    let tags = editing(&fx, &target, TableInfo::new("main", "tags"));
    assert_eq!(tags.key, ["item_id", "tag"]);
    assert_eq!(tags.foreign_keys[0].columns, ["item_id"]);

    let view = editing(&fx, &target, TableInfo { kind: TableKind::View, ..TableInfo::new("main", "v") });
    assert!(view.read_only.unwrap().contains("view"));
    assert!(view.key.is_empty());
}

#[test]
fn checks_then_applies_in_one_transaction() {
    let fx = Fixture::new();
    let target = setup(&fx);
    let table = TableInfo::new("main", "items");
    let s = &fx.service;

    // Text into a binary column, a partial key, an unknown column: refused before any SQL.
    let blob = [RowChange::Update { key: key(1), set: vec![set("data", EditValue::Text("0x00".into()))] }];
    assert!(matches!(block_on(s.preview_table_changes(&target, &table, &blob)), Err(Error::Unsupported(_))));
    let null_blob = [RowChange::Update { key: key(1), set: vec![set("data", EditValue::Null)] }];
    assert_eq!(block_on(s.preview_table_changes(&target, &table, &null_blob)).unwrap().len(), 1);
    let partial = [RowChange::Delete { key: vec![KeyValue { column: "item_id".into(), value: Value::Int(1) }] }];
    assert!(block_on(s.preview_table_changes(&target, &TableInfo::new("main", "tags"), &partial)).is_err());

    let changes = vec![
        RowChange::Update { key: key(1), set: vec![set("name", EditValue::Text("A".into()))] },
        RowChange::Insert { values: vec![set("id", EditValue::Default), set("name", EditValue::Null)] }, // NOT NULL: fails
    ];
    let error = block_on(s.apply_table_changes(&target, &table, changes, None)).unwrap_err();
    assert!(error.to_string().contains("Nothing was saved"), "{error}");
    let names = block_on(s.run(&target, "select name from items order by id".into(), RunOptions { record_history: false, ..Default::default() })).unwrap();
    assert_eq!(names.result.unwrap().rows, [[Value::Text("a".into())], [Value::Text("b".into())]]);

    let changes = vec![RowChange::Update { key: key(1), set: vec![set("name", EditValue::Text("A".into()))] }, RowChange::Delete { key: key(2) }];
    let reviewed: Vec<String> = block_on(s.preview_table_changes(&target, &table, &changes)).unwrap().into_iter().map(|s| s.sql).collect();
    assert!(block_on(s.apply_table_changes(&target, &table, changes.clone(), Some(&reviewed[..1]))).is_err());
    assert_eq!(block_on(s.apply_table_changes(&target, &table, changes, Some(&reviewed))).unwrap(), 2);
    let names = block_on(s.run(&target, "select name from items order by id".into(), RunOptions { record_history: false, ..Default::default() })).unwrap();
    assert_eq!(names.result.unwrap().rows, [[Value::Text("A".into())]]);
}
