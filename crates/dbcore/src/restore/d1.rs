//! Runs a script on Cloudflare D1, in batches: each request is all or nothing on D1's side, and
//! D1 refuses `BEGIN`/`COMMIT`, so there's no transaction across the whole script. The script's
//! own transaction statements are skipped, and each batch defers foreign key checks to its end
//! (`PRAGMA defer_foreign_keys`), so rows may come before the rows they point at within it.
//!
//! A failed batch was rolled back, and D1 doesn't say which statement failed: its statements run
//! again one at a time to find it (and, when not stopping at errors, to run the others).

use std::sync::LazyLock;

use regex::Regex;
use tokio::sync::mpsc;

use super::split::Item;
use super::{copy_unsupported, Tally};
use crate::d1::client::{Client, D1Stmt};
use crate::driver::{Error, Result};
use crate::model::{ConnectionConfig, DatabaseKind};

/// Statements per request, and the most SQL text sent at once.
const GROUP: usize = 100;
const GROUP_BYTES: usize = 256 * 1024;

/// Transaction control D1 refuses: skipped.
static TRANSACTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(begin|start\s+transaction|commit|end|rollback)(\s+(deferred|immediate|exclusive))?(\s+(transaction|work))?\s*;?$")
        .expect("valid regex")
});

pub(super) async fn run(config: &ConnectionConfig, mut rx: mpsc::Receiver<Result<Item>>, mut tally: Tally) -> Result<Tally> {
    let client = Client::new(config)?;
    if tally.options.single_transaction {
        // Not possible on D1: stop at the first error instead, and say what already ran stays.
        tally.options = super::RestoreOptions { single_transaction: false, stop_on_error: true };
        tally.summary.warnings.push(format!(
            "D1 can’t restore in one transaction: statements run in batches of up to {GROUP}, each all or nothing, and the restore stops at the first error"
        ));
    }
    let mut group: Vec<(String, usize)> = Vec::new();
    let mut bytes = 0;
    while let Some(item) = rx.recv().await {
        match item? {
            Item::Statement { sql, line } => {
                if TRANSACTION.is_match(sql.trim()) {
                    continue;
                }
                bytes += sql.len();
                group.push((sql, line));
                if group.len() >= GROUP || bytes >= GROUP_BYTES {
                    send(&client, &mut group, &mut tally).await?;
                    bytes = 0;
                }
            }
            Item::CopyStart { line, .. } => {
                send(&client, &mut group, &mut tally).await?;
                bytes = 0;
                tally.failed(line, copy_unsupported(DatabaseKind::D1))?;
            }
            Item::CopyData(_) | Item::CopyEnd => {}
            Item::Meta { command, line } => tally.meta(&command, line)?,
        }
    }
    send(&client, &mut group, &mut tally).await?;
    Ok(tally)
}

fn batch<'a>(statements: impl IntoIterator<Item = &'a str>) -> Vec<D1Stmt> {
    std::iter::once(D1Stmt::new("PRAGMA defer_foreign_keys = on")).chain(statements.into_iter().map(D1Stmt::new)).collect()
}

async fn send(client: &Client, group: &mut Vec<(String, usize)>, tally: &mut Tally) -> Result<()> {
    if group.is_empty() {
        return Ok(());
    }
    let statements: Vec<(String, usize)> = std::mem::take(group);
    match client.run(&batch(statements.iter().map(|(sql, _)| sql.as_str()))).await {
        Ok(_) => {
            for _ in &statements {
                tally.ran();
            }
            Ok(())
        }
        Err(failure) if matches!(failure.error, Error::Query(_)) => {
            // Rolled back: find the failing statement by running them one by one.
            for (sql, line) in &statements {
                match client.run(&batch([sql.as_str()])).await {
                    Ok(_) => tally.ran(),
                    Err(failure) => tally.failed(*line, failure.error)?,
                }
            }
            Ok(())
        }
        Err(failure) => Err(failure.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_transaction_control() {
        for sql in ["BEGIN TRANSACTION;", "begin immediate", "COMMIT;", "end transaction", "rollback;"] {
            assert!(TRANSACTION.is_match(sql), "{sql}");
        }
        assert!(!TRANSACTION.is_match("create table begin_x (a)"));
    }
}
