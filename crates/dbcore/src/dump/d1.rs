//! Cloudflare D1 dumps: the SQLite format (`sqlite3 .dump`), read over D1's REST API.
//!
//! D1 has no read transaction that spans requests, so tables are read one after another and a
//! dump of a database that's being written to may catch some changes and not others (the dump
//! says so in a warning). Rows are read in pages ordered by `rowid` (or the primary key of
//! `WITHOUT ROWID` tables), selected through `quote()` so values are exact. D1's own tables
//! (`_cf_*`) are left out. Row counts are never asked for (D1 bills rows read).

use super::libsql::write_value;
use super::sqlite::{insert_shape, plan, sequence_rows, statement, table_xinfo_query, Object, MASTER_QUERY};
use super::{Ctx, DumpPhase};
use crate::d1::client::{parse_literal, Client, D1Stmt};
use crate::dialect::Dialect;
use crate::driver::{Error, Result};
use crate::libsql::hrana::{HValue, StmtResult};
use crate::model::{ConnectionConfig, DatabaseKind};

const SQLITE: Dialect = Dialect(DatabaseKind::D1);

/// Rows per request: big enough to keep requests few, small enough for D1's response limits.
const PAGE: usize = 500;

fn text(value: Option<&HValue>) -> String {
    value.and_then(HValue::as_text).unwrap_or_default()
}

async fn query(client: &Client, sql: impl Into<String>) -> Result<StmtResult> {
    client.run(&[D1Stmt::new(sql)]).await?.pop().ok_or_else(|| Error::Internal("D1 returned no result".into()))
}

fn is_internal(name: &str) -> bool {
    name.starts_with("_cf_")
}

pub(super) async fn dump(config: &ConnectionConfig, ctx: &mut Ctx) -> Result<()> {
    let client = Client::new(config)?;
    let objects: Vec<Object> = query(&client, MASTER_QUERY)
        .await?
        .rows
        .iter()
        .map(|row| Object { kind: text(row.first()), name: text(row.get(1)), table: text(row.get(2)), sql: text(row.get(3)) })
        .filter(|o| !is_internal(&o.name) && !is_internal(&o.table))
        .collect();

    super::header(ctx, config, "");
    let options = ctx.options.clone();
    let plan = plan(&objects, &options);
    for warning in &plan.warnings {
        ctx.warn(warning.clone());
    }
    let content = options.content;
    if content.data() && plan.tables.len() > 1 {
        ctx.warn("D1 can’t read every table at the same moment: rows written during the dump may be in some tables and not others");
    }
    ctx.set_tables_total(if content.data() { plan.tables.len() as u32 } else { 0 });
    ctx.push(&plan.preamble(&options));

    ctx.phase(DumpPhase::Schema);
    for table in &plan.tables {
        if content.schema() {
            ctx.push(&statement(&table.sql));
        }
        if content.data() {
            ctx.phase(DumpPhase::Data);
            dump_rows(&client, ctx, table).await?;
        }
    }
    if plan.sequence {
        let rows: Vec<(String, i64)> = query(&client, "select name, seq from main.sqlite_sequence")
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
    Ok(())
}

/// How a table's pages are ordered and resumed: `rowid`, or the primary key (row values).
enum Order {
    RowId,
    Key(Vec<String>),
}

async fn dump_rows(client: &Client, ctx: &mut Ctx, table: &Object) -> Result<()> {
    let name = &table.name;
    let mut results = client
        .run(&[
            D1Stmt::new(table_xinfo_query(name)),
            D1Stmt::with_args("select name from pragma_table_info(?1) where pk > 0 order by pk", &[name]),
        ])
        .await?
        .into_iter();
    let (Some(xinfo), Some(keys)) = (results.next(), results.next()) else { return Err(Error::Internal("D1 returned too few results".into())) };
    let columns: Vec<(String, i64)> = xinfo.rows.iter().map(|row| (text(row.first()), row.get(1).and_then(HValue::as_i64).unwrap_or(0))).collect();
    let Some((prefix, _, width)) = insert_shape(name, &columns) else { return Ok(()) };
    let insertable: Vec<String> = columns.iter().filter(|(_, hidden)| *hidden == 0).map(|(n, _)| SQLITE.quote_ident(n)).collect();
    let keys: Vec<String> = keys.rows.iter().map(|row| SQLITE.quote_ident(&text(row.first()))).collect();
    let without_rowid = table.sql.to_ascii_lowercase().contains("without rowid");
    let shadowed = columns.iter().any(|(n, _)| ["rowid", "_rowid_", "oid"].iter().any(|r| n.eq_ignore_ascii_case(r)));
    let order = if without_rowid || shadowed { Order::Key(keys) } else { Order::RowId };
    let order_terms: Vec<String> = match &order {
        Order::RowId => vec!["rowid".into()],
        Order::Key(keys) => keys.clone(),
    };
    if order_terms.is_empty() {
        return Err(Error::Unsupported(format!("dumping “{name}”: it has no rowid and no primary key to page by")));
    }
    let select: Vec<String> = insertable.iter().chain(&order_terms).map(|c| format!("quote({c})")).collect();
    let relation = SQLITE.quote_relation("main", name);
    ctx.begin_table(name.clone(), None);

    let mut after: Option<Vec<String>> = None;
    let mut line = String::new();
    loop {
        let seek = after.as_ref().map(|values| {
            if values.len() == 1 {
                format!(" where {} > {}", order_terms[0], values[0])
            } else {
                format!(" where ({}) > ({})", order_terms.join(", "), values.join(", "))
            }
        });
        let sql = format!(
            "select {} from {relation}{} order by {} limit {PAGE}",
            select.join(", "),
            seek.unwrap_or_default(),
            order_terms.join(", ")
        );
        let page = query(client, sql).await?;
        for row in &page.rows {
            line.clear();
            line.push_str(&prefix);
            for (i, value) in row.iter().take(width).enumerate() {
                if i > 0 {
                    line.push(',');
                }
                write_value(&mut line, &parse_literal(&text(Some(value))));
            }
            line.push_str(");\n");
            ctx.push(&line);
        }
        ctx.rows(page.rows.len() as u64);
        ctx.tick().await?;
        if page.rows.len() < PAGE {
            break;
        }
        // The last row's sort key, already spelled as literals by `quote()`.
        let last = page.rows.last().expect("a full page");
        after = Some(last.iter().skip(width).map(|v| text(Some(v))).collect());
    }
    ctx.end_table();
    Ok(())
}
