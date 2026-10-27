//! Turso / libSQL dumps: the SQLite format (`sqlite3 .dump`), read over Hrana.
//!
//! Everything is read on one Hrana stream inside a read transaction, so every table comes from
//! the same snapshot; rows stream through a cursor. Row counts are never asked for (Turso bills
//! rows read), so a table's progress is indeterminate.

use std::sync::Arc;

use base64::Engine;

use super::literal::{sqlite_blob, sqlite_float, sqlite_text};
use super::sqlite::{insert_shape, plan, sequence_rows, statement, table_xinfo_query, Object, MASTER_QUERY};
use super::{Ctx, DumpPhase};
use crate::driver::{Error, Result};
use crate::libsql::hrana::{Batch, Client, CursorEntry, HValue, Stmt, StmtResult, Stream, StreamRequest, StreamResponse, StreamResult, BASE64};
use crate::model::ConnectionConfig;

/// Closes the stream (in the background) if the dump is dropped midway, so the server ends the
/// read transaction now instead of when the stream expires.
pub(crate) struct StreamGuard(pub Option<Stream>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take().filter(Stream::is_open) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(stream.close());
            }
        }
    }
}

impl StreamGuard {
    pub fn stream(&mut self) -> &mut Stream {
        self.0.as_mut().expect("open until dropped")
    }
}

/// Runs statements on the stream, one result (or error) each.
pub(crate) async fn execute(stream: &mut Stream, statements: impl IntoIterator<Item = Stmt>) -> Result<Vec<Result<StmtResult>>> {
    let requests: Vec<StreamRequest> = statements.into_iter().map(|stmt| StreamRequest::Execute { stmt }).collect();
    Ok(stream
        .pipeline(&requests)
        .await?
        .into_iter()
        .map(|result| match result {
            StreamResult::Ok { response: StreamResponse::Execute { result } } => Ok(result),
            StreamResult::Ok { .. } => Err(Error::Internal("unexpected response type".into())),
            StreamResult::Error { error } => Err(error.into_error()),
        })
        .collect())
}

async fn query(stream: &mut Stream, sql: &str) -> Result<StmtResult> {
    execute(stream, [Stmt::new(sql)]).await?.pop().expect("one result")
}

fn text(value: Option<&HValue>) -> String {
    value.and_then(HValue::as_text).unwrap_or_default()
}

pub(super) async fn dump(config: &ConnectionConfig, ctx: &mut Ctx) -> Result<()> {
    let client = Arc::new(Client::new(config)?);
    let mut guard = StreamGuard(Some(client.stream()));
    let stream = guard.stream();
    let [begin, version, master] = <[_; 3]>::try_from(
        execute(stream, [Stmt::new("begin"), Stmt::new("select sqlite_version()"), Stmt::new(MASTER_QUERY)]).await?,
    )
    .unwrap_or_else(|_| unreachable!("three statements"));
    begin?;
    let version = version.ok().and_then(|r| r.rows.first().map(|row| text(row.first()))).unwrap_or_default();
    let objects: Vec<Object> = master?
        .rows
        .iter()
        .map(|row| Object { kind: text(row.first()), name: text(row.get(1)), table: text(row.get(2)), sql: text(row.get(3)) })
        .collect();

    super::header(ctx, config, &format!("libSQL (SQLite {version})"));
    let options = ctx.options.clone();
    let plan = plan(&objects, &options);
    for warning in &plan.warnings {
        ctx.warn(warning.clone());
    }
    let content = options.content;
    ctx.set_tables_total(if content.data() { plan.tables.len() as u32 } else { 0 });
    ctx.push(&plan.preamble(&options));

    ctx.phase(DumpPhase::Schema);
    for table in &plan.tables {
        if content.schema() {
            ctx.push(&statement(&table.sql));
        }
        if content.data() {
            ctx.phase(DumpPhase::Data);
            dump_rows(stream, ctx, &table.name).await?;
        }
    }
    if plan.sequence {
        let rows: Vec<(String, i64)> = query(stream, "select name, seq from main.sqlite_sequence")
            .await?
            .rows
            .iter()
            .map(|row| (text(row.first()), row.get(1).and_then(HValue::as_i64).unwrap_or(0)))
            .collect();
        ctx.push(&sequence_rows(&plan, &rows));
    }
    if content.schema() {
        ctx.phase(DumpPhase::PostData);
        for object in &plan.post_data {
            ctx.push(&statement(&object.sql));
        }
    }
    ctx.line("COMMIT;");

    let mut stream = guard.0.take().expect("open");
    let _ = execute(&mut stream, [Stmt::new("rollback")]).await;
    stream.close().await;
    Ok(())
}

async fn dump_rows(stream: &mut Stream, ctx: &mut Ctx, table: &str) -> Result<()> {
    let columns: Vec<(String, i64)> = query(stream, &table_xinfo_query(table))
        .await?
        .rows
        .iter()
        .map(|row| (text(row.first()), row.get(1).and_then(HValue::as_i64).unwrap_or(0)))
        .collect();
    let Some((prefix, select, _)) = insert_shape(table, &columns) else { return Ok(()) };
    ctx.begin_table(table.to_string(), None);

    let mut cursor = stream.cursor(&Batch::chained([Stmt::new(select)])).await?;
    let mut line = String::new();
    while let Some(entry) = cursor.next().await? {
        match entry {
            CursorEntry::Row { row } => {
                line.clear();
                line.push_str(&prefix);
                for (i, value) in row.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    write_value(&mut line, value);
                }
                line.push_str(");\n");
                ctx.push(&line);
                ctx.rows(1);
                ctx.tick().await?;
            }
            CursorEntry::StepError { error, .. } | CursorEntry::Error { error } => return Err(error.into_error()),
            _ => {}
        }
    }
    ctx.end_table();
    Ok(())
}

/// A Hrana value as a SQLite literal (same spelling as the SQLite dumper).
pub(super) fn write_value(out: &mut String, value: &HValue) {
    match value {
        HValue::Null => out.push_str("NULL"),
        HValue::Integer { value } => match value.parse::<i64>() {
            Ok(n) => out.push_str(&n.to_string()),
            Err(_) => out.push_str("NULL"),
        },
        HValue::Float { value } => out.push_str(&sqlite_float(*value)),
        HValue::Text { value } => sqlite_text(out, value.as_bytes()),
        HValue::Blob { base64 } => sqlite_blob(out, &BASE64.decode(base64.trim()).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_hrana_values() {
        let mut out = String::new();
        for value in [
            HValue::Null,
            HValue::Integer { value: "-9223372036854775808".into() },
            HValue::Float { value: 1.0 },
            HValue::Text { value: "it's\0".into() },
            HValue::Blob { base64: "AP8=".into() },
        ] {
            write_value(&mut out, &value);
            out.push('|');
        }
        assert_eq!(out, "NULL|-9223372036854775808|1.0|CAST(X'6974277300' AS TEXT)|X'00ff'|");
    }
}
