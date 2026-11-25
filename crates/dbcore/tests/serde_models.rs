//! The JSON wire encoding of the core's models (what `dbear-cli serve` and other clients speak):
//! exact shapes for the types the protocol is built on, and lossless round trips.

use dbcore::access::{AccessChange, GrantObject, RoleRef, RoleSpec};
use dbcore::complete::{CompletionItem, CompletionKind, Completions};
use dbcore::dump::{DumpContent, DumpOptions, DumpPhase, DumpProgress, DumpScope};
use dbcore::edit::{CellEdit, EditValue, KeyValue, RowChange};
use dbcore::restore::RestoreOptions;
use dbcore::state::HistoryEntry;
use dbcore::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) -> serde_json::Value {
    let json = serde_json::to_value(value).unwrap();
    let text = serde_json::to_string(value).unwrap();
    assert_eq!(&serde_json::from_str::<T>(&text).unwrap(), value, "{text}");
    json
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

#[test]
fn value_encoding() {
    let max_safe = (1i64 << 53) - 1;
    let cases = [
        (Value::Null, json!(null)),
        (Value::Bool(true), json!(true)),
        (Value::Int(42), json!(42)),
        (Value::Int(-max_safe), json!(-max_safe)),
        (Value::Int(max_safe + 1), json!({"int": "9007199254740992"})),
        (Value::Int(i64::MIN), json!({"int": "-9223372036854775808"})),
        (Value::Int(i64::MAX), json!({"int": "9223372036854775807"})),
        (Value::Float(1.5), json!({"float": 1.5})),
        (Value::Float(1.0), json!({"float": 1.0})),
        (Value::Float(f64::INFINITY), json!({"float": "Infinity"})),
        (Value::Float(f64::NEG_INFINITY), json!({"float": "-Infinity"})),
        (Value::Decimal("12.30".into()), json!({"decimal": "12.30"})),
        (Value::Decimal("123456789012345678901234567890.000000001".into()), json!({"decimal": "123456789012345678901234567890.000000001"})),
        (Value::Text("hi".into()), json!("hi")),
        (Value::Text("0x0102".into()), json!("0x0102")),
        (Value::Text(String::new()), json!("")),
    ];
    for (value, expected) in cases {
        assert_eq!(round_trip(&value), expected, "{value:?}");
    }
    // Text that looks like a number or a tag stays text.
    assert_eq!(round_trip(&Value::Text("42".into())), json!("42"));
    assert_eq!(round_trip(&Value::Text("{\"int\":\"1\"}".into())), json!("{\"int\":\"1\"}"));
}

#[test]
fn value_floats_are_exact() {
    for f in [0.1, -0.0, f64::MIN_POSITIVE, f64::MAX, f64::MIN, 1e-300, 2f64.powi(60), std::f64::consts::PI] {
        let text = serde_json::to_string(&Value::Float(f)).unwrap();
        let Value::Float(back) = serde_json::from_str(&text).unwrap() else { panic!("{text}") };
        assert_eq!(back.to_bits(), f.to_bits(), "{text}");
    }
    let Value::Float(nan) = serde_json::from_str(r#"{"float":"NaN"}"#).unwrap() else { panic!() };
    assert!(nan.is_nan());
    assert_eq!(serde_json::to_string(&Value::Float(f64::NAN)).unwrap(), r#"{"float":"NaN"}"#);
}

#[test]
fn value_lenient_reading() {
    let read = |s: &str| serde_json::from_str::<Value>(s);
    assert_eq!(read("9007199254740993").unwrap(), Value::Int(9007199254740993));
    assert_eq!(read(r#"{"int": 7}"#).unwrap(), Value::Int(7));
    assert_eq!(read("2.5").unwrap(), Value::Float(2.5));
    assert_eq!(read(r#"{"float": 3}"#).unwrap(), Value::Float(3.0));
    assert!(read(r#"{"int": "x"}"#).is_err());
    assert!(read(r#"{"decimal": 1}"#).is_err());
    assert!(read(r#"{"blob": "00"}"#).is_err());
    assert!(read(r#"{"int": 1, "float": 2}"#).is_err());
    assert!(read("{}").is_err());
    assert!(read("[1]").is_err());
    assert!(read("18446744073709551615").is_err());
}

#[test]
fn error_encoding() {
    assert_eq!(round_trip(&Error::Query("syntax error".into())), json!({"kind": "query", "message": "syntax error"}));
    assert_eq!(
        round_trip(&Error::ConnectionFailed("refused".into())),
        json!({"kind": "connection-failed", "message": "refused"})
    );
    assert_eq!(round_trip(&Error::Cancelled), json!({"kind": "cancelled"}));
    for e in [
        Error::TableNotFound("t".into()),
        Error::Unsupported("u".into()),
        Error::InvalidConfig("c".into()),
        Error::Storage("s".into()),
        Error::Internal("i".into()),
    ] {
        round_trip(&e);
    }
}

fn config() -> ConnectionConfig {
    ConnectionConfig {
        id: "8a3f".into(),
        name: "app".into(),
        group: "Work".into(),
        kind: DatabaseKind::Postgres,
        host: "db.internal".into(),
        port: Some(5432),
        database: "app_dev".into(),
        user: Some("app".into()),
        password: Some("hunter2".into()),
        ssl_mode: SslMode::VerifyFull,
        show_all_databases: true,
        ssh: Some(SshTunnel {
            host: "bastion".into(),
            port: None,
            user: "me".into(),
            auth: SshAuth::PrivateKey,
            key_path: "~/.ssh/id_ed25519".into(),
            secret: Some("passphrase".into()),
            forwarded_port: Some(40000),
        }),
    }
}

#[test]
fn connection_config_never_serializes_secrets() {
    let json = serde_json::to_value(config()).unwrap();
    assert_eq!(
        json,
        json!({
            "id": "8a3f", "name": "app", "group": "Work", "kind": "postgres", "host": "db.internal", "port": 5432,
            "database": "app_dev", "user": "app", "ssl_mode": "verify-full", "show_all_databases": true,
            "ssh": {"host": "bastion", "port": null, "user": "me", "auth": "private-key", "key_path": "~/.ssh/id_ed25519"}
        })
    );
    let text = json.to_string();
    assert!(!text.contains("hunter2") && !text.contains("passphrase") && !text.contains("40000"));

    // Read back: secrets absent → None; the rest is equal.
    let back: ConnectionConfig = serde_json::from_value(json).unwrap();
    let mut expected = config();
    expected.password = None;
    let ssh = expected.ssh.as_mut().unwrap();
    ssh.secret = None;
    ssh.forwarded_port = None;
    assert_eq!(back, expected);

    // A client may pass secrets in; a forwarded port is never taken.
    let given: ConnectionConfig = serde_json::from_value(json!({
        "id": "x", "name": "x", "kind": "sqlserver", "password": "pw",
        "ssh": {"host": "h", "secret": "s", "forwarded_port": 1}
    }))
    .unwrap();
    assert_eq!(given.password.as_deref(), Some("pw"));
    assert_eq!(given.ssh.as_ref().unwrap().secret.as_deref(), Some("s"));
    assert_eq!(given.ssh.as_ref().unwrap().forwarded_port, None);
    assert_eq!(given.ssl_mode, SslMode::Prefer);
    assert!(given.host.is_empty() && given.port.is_none() && !given.show_all_databases);
}

#[test]
fn tables_and_structure_round_trip() {
    let mut view = TableInfo::new("public", "active_users");
    view.kind = TableKind::View;
    view.estimated_row_count = Some(12);
    let schema = Schema { name: "public".into(), tables: vec![TableInfo::new("public", "users"), view] };
    assert_eq!(round_trip(&schema)["tables"][1]["kind"], json!("view"));

    let column = ColumnInfo { name: "id".into(), type_name: "int8".into(), is_primary_key: true, is_nullable: false };
    round_trip(&TableColumns { schema: "public".into(), table: "users".into(), columns: vec![column.clone()] });
    let query = RowQuery { sort: vec![SortKey { column: "id".into(), descending: true }], filter: Some("id > 1".into()) };
    assert_eq!(round_trip(&query), json!({"sort": [{"column": "id", "descending": true}], "filter": "id > 1"}));
    assert_eq!(serde_json::from_str::<RowQuery>("{}").unwrap(), RowQuery::default());

    round_trip(&TableStructure {
        columns: vec![ColumnDetail {
            name: "id".into(),
            type_name: "bigint".into(),
            is_nullable: false,
            default_value: Some("nextval('users_id_seq'::regclass)".into()),
            is_primary_key: true,
            comment: None,
        }],
        primary_key: vec!["id".into()],
        indexes: vec![IndexInfo { name: "users_pkey".into(), columns: vec!["id".into()], is_unique: true, is_primary: true, definition: None }],
        foreign_keys: vec![ForeignKeyInfo {
            name: "fk".into(),
            columns: vec!["org_id".into()],
            referenced_schema: "public".into(),
            referenced_table: "orgs".into(),
            referenced_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
        }],
        referenced_by: vec![ReferencingKey {
            schema: "public".into(),
            table: "orders".into(),
            name: "orders_user_fk".into(),
            columns: vec!["user_id".into()],
            referenced_columns: vec![],
        }],
        ddl: Some("create table users (…)".into()),
    });

    let result = QueryResult {
        columns: vec![column],
        rows: vec![vec![Value::Int(1)], vec![Value::Int(i64::MAX)], vec![Value::Null]],
        total_count: Some(3),
        truncated: false,
        rows_affected: None,
        origins: vec![Some(ColumnOrigin { schema: "public".into(), table: "users".into(), column: "id".into() })],
    };
    let json = round_trip(&result);
    assert_eq!(json["rows"], json!([[1], [{"int": "9223372036854775807"}], [null]]));
}

#[test]
fn row_page_cursor_is_an_opaque_string() {
    let table = TableInfo::new("main", "t");
    let query = RowQuery::default();
    let page = RowPage { result: QueryResult::default(), next: Some(PageCursor::offset(&table, &query, 500)) };
    let json = round_trip(&page);
    let token = json["next"].as_str().expect("a string");
    assert_eq!(PageCursor::decode(token).unwrap().rows_before(), 500);
    assert_eq!(round_trip(&RowPage::default())["next"], json!(null));
    assert!(serde_json::from_value::<RowPage>(json!({"result": {}, "next": "garbage"})).is_err());
    assert_eq!(serde_json::from_value::<RowPage>(json!({"result": {}})).unwrap(), RowPage::default());
}

/// A keyset cursor from a real SQLite page survives the trip and continues the same paging.
#[test]
fn sqlite_keyset_page_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "create table t (id integer primary key, big int, f real, n numeric, b blob);
         insert into t values (1, 9223372036854775807, 0.1, 1.25, X'0102'), (2, -9007199254740993, 1e300, null, null), (3, 3, null, 7, X'');",
    )
    .unwrap();
    drop(db);
    let mut config = ConnectionConfig::new_empty(DatabaseKind::Sqlite);
    config.id = "serde-test".into();
    config.database = path.display().to_string();
    let conn = Connection::new(config);
    let table = TableInfo::new("main", "t");
    let query = RowQuery::default();

    let mut rows = Vec::new();
    let mut after = None;
    let mut seeked = false;
    loop {
        let page = block_on(conn.fetch_page(table.clone(), query.clone(), 1, after)).unwrap();
        let page: RowPage = serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
        rows.extend(page.result.rows);
        seeked |= page.next.as_ref().is_some_and(PageCursor::is_keyset);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert!(seeked);
    let all = block_on(conn.execute("select * from t order by id".into())).unwrap();
    let all: QueryResult = serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
    assert_eq!(rows, all.rows);
    assert_eq!(rows.len(), 3);
}

#[test]
fn history_dump_restore_and_edits_round_trip() {
    let entry = HistoryEntry {
        id: 3,
        connection_id: "8a3f".into(),
        database: String::new(),
        sql: "select 1".into(),
        ran_at: 1_700_000_000_000,
        duration_ms: Some(12),
        rows: Some(1),
        error: None,
    };
    round_trip(&entry);

    let options = DumpOptions {
        content: DumpContent::SchemaOnly,
        scope: DumpScope::Tables(vec![TableInfo::new("public", "users")]),
        drop_objects: true,
        ..Default::default()
    };
    let json = round_trip(&options);
    assert_eq!(json["content"], json!("schema-only"));
    assert_eq!(json["scope"]["tables"][0]["name"], json!("users"));
    assert_eq!(json["compression"], json!("none"));
    assert_eq!(json["data_style"], json!("copy"));
    assert_eq!(round_trip(&DumpScope::Database), json!("database"));
    assert_eq!(round_trip(&DumpScope::Schemas(vec!["a".into()])), json!({"schemas": ["a"]}));
    assert_eq!(serde_json::from_str::<DumpOptions>("{}").unwrap(), DumpOptions::default());
    round_trip(&DumpProgress {
        phase: DumpPhase::PostData,
        object: Some("public.users".into()),
        tables_done: 1,
        tables_total: 2,
        rows_done: 10,
        table_rows_done: 5,
        table_rows_estimate: None,
        bytes_written: 100,
    });
    assert_eq!(serde_json::from_str::<RestoreOptions>("{}").unwrap(), RestoreOptions::default());

    let change = RowChange::Update {
        key: vec![KeyValue { column: "id".into(), value: Value::Int(5) }],
        set: vec![CellEdit { column: "name".into(), value: EditValue::Text("Ana".into()) }, CellEdit { column: "x".into(), value: EditValue::Default }],
    };
    assert_eq!(
        round_trip(&change),
        json!({"kind": "update", "key": [{"column": "id", "value": 5}], "set": [{"column": "name", "value": {"text": "Ana"}}, {"column": "x", "value": "default"}]})
    );
    round_trip(&RowChange::Delete { key: vec![] });

    let completions = Completions {
        replace_start: 0,
        replace_end: 2,
        items: vec![CompletionItem { label: "users".into(), insert_text: "users".into(), kind: CompletionKind::Table, detail: None }],
    };
    assert_eq!(round_trip(&completions)["items"][0]["kind"], json!("table"));

    let grant = AccessChange::SetPrivileges {
        role: RoleRef::new("app", None),
        object: GrantObject::Table { schema: "public".into(), name: "users".into() },
        before: Default::default(),
        after: Default::default(),
    };
    let json = round_trip(&grant);
    assert_eq!(json["kind"], json!("set-privileges"));
    assert_eq!(json["object"], json!({"kind": "table", "schema": "public", "name": "users"}));
    round_trip(&AccessChange::CreateRole(RoleSpec { name: "r".into(), ..Default::default() }));
    round_trip(&AccessChange::DropRole(RoleRef::new("r", Some("%".into()))));
}
