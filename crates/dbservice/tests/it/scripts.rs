use std::time::Duration;

use dbcore::{ColumnInfo, Error, QueryResult, Value};
use dbservice::{history_entry, RunOptions, ScriptOutcome, Target, SCRIPT_ROW_LIMIT};

use crate::{block_on, Fixture};

fn setup(fx: &Fixture) -> Target {
    let config = fx.sqlite("app", "app.db");
    let target = Target::default_for(&config);
    let sql = "create table items (id integer primary key, name text);
               insert into items (name) values ('a'), ('b'), ('c'), ('d'), ('e');";
    block_on(fx.service.run(&target, sql.into(), RunOptions { record_history: false, ..Default::default() })).unwrap().result.unwrap();
    target
}

#[test]
fn run_options_default_to_the_apps() {
    let options = RunOptions::default();
    assert_eq!(options.max_rows, Some(SCRIPT_ROW_LIMIT));
    assert_eq!(SCRIPT_ROW_LIMIT, 10_000);
    assert!(options.record_history && !options.describe_sources);
    assert_eq!(serde_json::from_str::<RunOptions>("{}").unwrap(), options);
    let none: RunOptions = serde_json::from_str(r#"{"max_rows":null,"record_history":false}"#).unwrap();
    assert_eq!(none, RunOptions { max_rows: None, record_history: false, describe_sources: false });
}

#[test]
fn records_runs_in_the_history() {
    let fx = Fixture::new();
    let target = setup(&fx);
    assert_eq!(fx.service.history(&target.connection_id, 10).unwrap(), []);

    let select = block_on(fx.service.run(&target, "select * from items".into(), RunOptions::default())).unwrap();
    assert_eq!(select.result.as_ref().unwrap().rows.len(), 5);
    assert!(!select.schema_changed && select.history_error.is_none() && select.sources.is_none());
    let update = block_on(fx.service.run(&target, "update items set name = 'z' where id < 3".into(), RunOptions::default())).unwrap();
    assert_eq!(update.result.as_ref().unwrap().rows_affected, Some(2));
    let failed = block_on(fx.service.run(&target, "select * from nope".into(), RunOptions::default())).unwrap();
    let Err(error) = &failed.result else { panic!() };
    let quiet = block_on(fx.service.run(&target, "select 1".into(), RunOptions { record_history: false, ..Default::default() })).unwrap();
    assert!(quiet.result.is_ok());

    let history = fx.service.history(&target.connection_id, 10).unwrap();
    let summary: Vec<(&str, Option<u64>, Option<&str>)> = history.iter().map(|h| (h.sql.as_str(), h.rows, h.error.as_deref())).collect();
    let message = error.to_string();
    assert_eq!(
        summary,
        [
            ("select * from nope", None, Some(message.as_str())),
            ("update items set name = 'z' where id < 3", Some(2), None),
            ("select * from items", Some(5), None),
        ]
    );
    assert!(history.iter().all(|h| h.database == target.database && h.connection_id == target.connection_id && h.duration_ms.is_some()));
    assert_eq!(fx.service.history(&target.connection_id, 1).unwrap().len(), 1);
    fx.service.clear_history(&target.connection_id).unwrap();
    assert_eq!(fx.service.history(&target.connection_id, 10).unwrap(), []);
}

#[test]
fn caps_rows_and_counts_what_was_there() {
    let fx = Fixture::new();
    let target = setup(&fx);
    let outcome = block_on(fx.service.run(&target, "select * from items".into(), RunOptions { max_rows: Some(2), ..Default::default() })).unwrap();
    let result = outcome.result.unwrap();
    assert_eq!(result.rows.len(), 2);
    assert!(result.truncated);
    let entry = &fx.service.history(&target.connection_id, 1).unwrap()[0];
    assert_eq!(entry.rows, Some(result.total_count.unwrap_or(2)));
}

#[test]
fn history_entry_follows_the_apps_rule() {
    let target = Target::new("c", "db");
    let took = Duration::from_millis(12);
    let rows = |columns: usize, rows: usize, total: Option<u64>, affected: Option<u64>| QueryResult {
        columns: (0..columns).map(|i| ColumnInfo { name: format!("c{i}"), type_name: String::new(), is_primary_key: false, is_nullable: true }).collect(),
        rows: (0..rows).map(|_| vec![Value::Null; columns]).collect(),
        total_count: total,
        rows_affected: affected,
        ..Default::default()
    };
    assert_eq!(history_entry(&target, "x", &Ok(rows(0, 0, None, Some(4))), took).unwrap().rows, Some(4));
    assert_eq!(history_entry(&target, "x", &Ok(rows(2, 3, None, Some(9))), took).unwrap().rows, Some(3));
    assert_eq!(history_entry(&target, "x", &Ok(rows(2, 3, Some(40), None)), took).unwrap().rows, Some(40));
    let failed = history_entry(&target, "x", &Err(Error::Query("boom".into())), took).unwrap();
    assert_eq!((failed.rows, failed.error.as_deref()), (None, Some(Error::Query("boom".into()).to_string().as_str())));
    assert_eq!((failed.connection_id.as_str(), failed.database.as_str(), failed.duration), ("c", "db", Some(took)));
    assert!(history_entry(&target, "x", &Err(Error::Cancelled), took).is_none());
}

#[test]
fn cancelled_runs_arent_recorded_nor_refresh() {
    let fx = Fixture::new();
    let target = setup(&fx);
    // Its own session, as a JSON-RPC client has: cancel reaches only this one.
    let connection = block_on(fx.service.new_connection(&target)).unwrap();
    block_on(fx.service.load(&target, false)).unwrap();
    let canceller = {
        let connection = connection.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            block_on(connection.cancel());
        })
    };
    // Endless, and mentions CREATE so it would otherwise count as DDL.
    let sql = "with recursive n(x) as (select 1 union all select x + 1 from n) select count(*) as created from n";
    let outcome = block_on(fx.service.run_script(&target, &connection, sql.into(), RunOptions::default()));
    canceller.join().unwrap();
    assert!(outcome.is_cancelled(), "{:?}", outcome.result);
    assert!(!outcome.schema_changed);
    assert!(fx.service.cached_schemas(&target).is_some());
    assert_eq!(fx.service.history(&target.connection_id, 10).unwrap(), []);
}

#[test]
fn describes_where_columns_come_from() {
    let fx = Fixture::new();
    let target = setup(&fx);
    let options = RunOptions { describe_sources: true, ..Default::default() };
    let outcome = block_on(fx.service.run(&target, "select id, name from items".into(), options.clone())).unwrap();
    let sources = outcome.sources.expect("sources");
    assert_eq!(sources.tables.len(), 1);
    assert_eq!(sources.columns, [Some((0, "id".to_string())), Some((0, "name".to_string()))]);
    let computed = block_on(fx.service.run(&target, "select 1 + 1".into(), options)).unwrap();
    // Like the apps: described whenever the driver reports origins, here none from a table.
    assert!(computed.sources.is_none_or(|s| s.tables.is_empty()));
}

#[test]
fn outcomes_serialize_as_ok_or_error() {
    let fx = Fixture::new();
    let target = setup(&fx);
    let ok = block_on(fx.service.run(&target, "select name from items where id = 1".into(), RunOptions::default())).unwrap();
    let json = serde_json::to_value(&ok).unwrap();
    assert_eq!(json["ok"], serde_json::Value::Null, "nested, not flattened");
    assert_eq!(json["result"]["ok"]["rows"], serde_json::json!([["a"]]));
    assert!(json["duration_ms"].is_u64());
    assert_eq!(json["schema_changed"], false);
    assert!(json.get("history_error").is_none());
    let back: ScriptOutcome = serde_json::from_value(json).unwrap();
    assert_eq!(back.result, ok.result);

    let failed = block_on(fx.service.run(&target, "select * from nope".into(), RunOptions::default())).unwrap();
    let json = serde_json::to_value(&failed).unwrap();
    assert_eq!(json["result"]["error"]["kind"], "query");
    let back: ScriptOutcome = serde_json::from_value(json).unwrap();
    assert_eq!(back.result, failed.result);

    let cancelled = serde_json::json!({"result": {"error": {"kind": "cancelled"}}, "duration_ms": 5, "schema_changed": false, "sources": null});
    let back: ScriptOutcome = serde_json::from_value(cancelled).unwrap();
    assert!(back.is_cancelled());
    assert_eq!(back.duration, Duration::from_millis(5));
}

#[test]
fn without_a_state_file_runs_still_work() {
    let fx = Fixture::with(|o| {
        let dir = o.store_path.as_ref().unwrap().parent().unwrap().to_path_buf();
        std::fs::write(dir.join("blocker"), "").unwrap();
        o.state_path = Some(dir.join("blocker/state.db"));
    });
    let target = setup(&fx);
    let outcome = block_on(fx.service.run(&target, "select 1".into(), RunOptions::default())).unwrap();
    assert!(outcome.result.is_ok() && outcome.history_error.is_none());
}
