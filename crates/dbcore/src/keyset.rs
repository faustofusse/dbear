//! Keyset ("seek") paging for table browsing.
//!
//! OFFSET paging makes the database read and discard every row before the page, so deep pages
//! get slower the further you scroll. Keyset paging instead remembers the sort-key values of the
//! last row shown and asks for rows that come *after* them:
//!
//! ```sql
//! select * from t where (filter) and ((total, id) > (19.90, 4211)) order by total, id limit 501
//! ```
//!
//! The order is always the user's sort plus a unique tiebreak (primary key, a unique NOT NULL
//! index, `ctid` or `rowid`), so "after" is well defined. NULLs sort where the engine puts them by
//! default ([`NullOrder`]); no `NULLS FIRST/LAST` is emitted, which MySQL and SQL Server lack.
//!
//! Drivers that can't seek a table safely (views, keyless tables, sort types whose comparison
//! differs from their ordering…) fall back to OFFSET: a [`PageCursor`] always knows how many rows
//! came before it. [`crate::Driver::fetch_page`]'s default implementation is OFFSET-only.

use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::driver::{Error, Result};
use crate::model::{ColumnInfo, DatabaseKind, QueryResult, RowQuery, SortKey, TableInfo};

/// A sort-key value of the last row of a page, taken from the wire before decoding, so it can be
/// sent back exactly (display values cut blobs and reformat numbers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CursorValue {
    Null,
    Int(i64),
    /// `f64::to_bits`: JSON can't hold infinities.
    Float(u64),
    Text(String),
    Bytes(Vec<u8>),
}

impl CursorValue {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn float(f: f64) -> Self {
        Self::Float(f.to_bits())
    }
}

/// Where the next page starts. Opaque to frontends: they get it from one page and pass it back
/// for the next one ([`PageCursor::encode`] makes it a string for FFI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageCursor {
    /// Table + sort + filter it was made for. A cursor for anything else restarts from `rows_before`.
    fingerprint: u64,
    /// Rows already loaded: the OFFSET of the next page when seeking isn't possible.
    rows_before: u64,
    seek: Option<Seek>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Seek {
    /// The ORDER BY terms the values belong to (they change if the table is altered).
    layout: u64,
    values: Vec<CursorValue>,
}

impl PageCursor {
    /// A cursor that just skips `rows_before` rows (OFFSET paging).
    pub fn offset(table: &TableInfo, query: &RowQuery, rows_before: u64) -> Self {
        Self { fingerprint: fingerprint(table, query), rows_before, seek: None }
    }

    pub fn rows_before(&self) -> u64 {
        self.rows_before
    }

    /// Whether the next page will seek (rather than use OFFSET), as far as the cursor knows.
    pub fn is_keyset(&self) -> bool {
        self.seek.is_some()
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("cursors serialize")
    }

    pub fn decode(token: &str) -> Result<Self> {
        serde_json::from_str(token).map_err(|e| Error::Internal(format!("bad page cursor: {e}")))
    }

    /// The OFFSET to use when a driver can't (or won't) seek with this cursor.
    pub fn offset_for(after: Option<&Self>, table: &TableInfo, query: &RowQuery) -> u64 {
        // A cursor from another query means nothing here: start over rather than skip random rows.
        after.filter(|c| c.fingerprint == fingerprint(table, query)).map_or(0, |c| c.rows_before)
    }
}

/// One page of a table and where the next one starts (`None`: this was the last page).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowPage {
    pub result: QueryResult,
    pub next: Option<PageCursor>,
}

impl RowPage {
    /// Builds an OFFSET page from `limit + 1` fetched rows starting at `offset`.
    pub fn from_offset(mut result: QueryResult, table: &TableInfo, query: &RowQuery, limit: u32, offset: u64) -> Self {
        let more = result.rows.len() > limit as usize;
        result.rows.truncate(limit as usize);
        if offset > 0 {
            result.total_count = None;
        }
        let next = more.then(|| PageCursor::offset(table, query, offset + u64::from(limit)));
        Self { result, next }
    }
}

fn fingerprint(table: &TableInfo, query: &RowQuery) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&table.schema, &table.name, &query.sort, &query.filter).hash(&mut h);
    h.finish()
}

/// Where each engine sorts NULLs by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullOrder {
    /// Postgres: NULLs after everything ascending, first descending.
    Largest,
    /// MySQL, SQLite, SQL Server: NULLs first ascending, last descending.
    Smallest,
}

impl NullOrder {
    pub fn of(kind: DatabaseKind) -> Self {
        match kind {
            DatabaseKind::Postgres => Self::Largest,
            _ => Self::Smallest,
        }
    }

    /// Whether NULLs come after the non-NULL values in this direction.
    fn last(self, descending: bool) -> bool {
        (self == Self::Largest) != descending
    }
}

/// Whether `(a, b) > (x, y)` is supported and used for index range scans. MySQL parses it but
/// scans the whole index (only `IN` lists of rows get ranges); SQL Server has no row values.
fn supports_row_values(kind: DatabaseKind) -> bool {
    matches!(kind, DatabaseKind::Postgres | DatabaseKind::Sqlite)
}

/// One `ORDER BY` term of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekColumn {
    /// Quoted column, or a row id like `ctid`/`rowid`.
    pub expr: String,
    pub descending: bool,
    pub nullable: bool,
    /// Position of its value in each fetched row (columns, then extra selected terms).
    pub index: usize,
}

impl SeekColumn {
    /// An ascending, NOT NULL tiebreak on a table column (primary key, unique index).
    pub fn column(dialect: Dialect, columns: &[ColumnInfo], name: &str) -> Option<Self> {
        let index = columns.iter().position(|c| c.name == name)?;
        Some(Self { expr: dialect.quote_ident(name), descending: false, nullable: false, index })
    }

    /// An ascending row id selected after the table's columns (`select *, ctid`).
    pub fn row_id(expr: &str, index: usize) -> Self {
        Self { expr: expr.into(), descending: false, nullable: false, index }
    }
}

/// How one table is paged: its ORDER BY terms, and whether a cursor can seek on them.
#[derive(Debug, Clone)]
pub struct Keyset {
    pub columns: Vec<SeekColumn>,
    /// Seek when a cursor allows it; otherwise every page uses OFFSET.
    pub enabled: bool,
    kind: DatabaseKind,
    fingerprint: u64,
    layout: u64,
}

/// How a page's rows are selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// `offset n` (0 for the first page).
    Offset(u64),
    /// Conditions for the rows after the cursor, in page order: query each in turn (`where … and
    /// (condition)`) until the page is full. Never empty.
    Seek(Vec<String>),
    /// Nothing can come after the cursor.
    Empty,
}

impl Keyset {
    /// The user's sort, then the `tiebreak` terms not sorted on yet (like [`Dialect::order_by`], but
    /// in the direction of the last sort key).
    /// Fails on columns the table doesn't have. `enabled` is the driver's call: only when the
    /// tiebreak is unique and every term compares the way it sorts.
    pub fn new(
        dialect: Dialect,
        table: &TableInfo,
        query: &RowQuery,
        columns: &[ColumnInfo],
        tiebreak: Vec<SeekColumn>,
        enabled: bool,
    ) -> Result<Self> {
        let mut terms: Vec<SeekColumn> = Vec::with_capacity(query.sort.len() + tiebreak.len());
        for SortKey { column, descending } in &query.sort {
            let index = columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| Error::Query(format!("Can’t sort by “{column}”: no such column")))?;
            let expr = dialect.quote_ident(column);
            if terms.iter().any(|t| t.expr == expr) {
                continue;
            }
            terms.push(SeekColumn { expr, descending: *descending, nullable: columns[index].is_nullable, index });
        }
        // The tiebreak runs the same way as the last sort key: `created_at desc, id desc` can be read
        // backwards off an index on `created_at`, while `created_at desc, id` needs a sort (MySQL).
        let descending = query.sort.last().is_some_and(|k| k.descending);
        for t in tiebreak {
            if !terms.iter().any(|u| u.expr == t.expr) {
                terms.push(SeekColumn { descending, ..t });
            }
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for t in &terms {
            (&t.expr, t.descending, t.index).hash(&mut h);
        }
        // A seek needs a unique last term: without a tiebreak, rows with equal keys would be skipped.
        let unique = terms.last().is_some_and(|t| !t.nullable);
        Ok(Self {
            enabled: enabled && unique,
            columns: terms,
            kind: dialect.0,
            fingerprint: fingerprint(table, query),
            layout: h.finish(),
        })
    }

    pub fn order_by(&self) -> Vec<String> {
        self.columns.iter().map(|c| if c.descending { format!("{} desc", c.expr) } else { c.expr.clone() }).collect()
    }

    /// Row positions of the key values to capture from each fetched row.
    pub fn key_indexes(&self) -> Vec<usize> {
        self.columns.iter().map(|c| c.index).collect()
    }

    /// How to fetch the page after `after`. `render` spells a key value (the index is the term's)
    /// as a literal or a bind parameter.
    pub fn start(&self, after: Option<&PageCursor>, render: impl FnMut(usize, &CursorValue) -> String) -> Start {
        let Some(cursor) = after.filter(|c| c.fingerprint == self.fingerprint) else { return Start::Offset(0) };
        match &cursor.seek {
            Some(seek) if self.enabled && seek.layout == self.layout && seek.values.len() == self.columns.len() => {
                let segments = seek_segments(self.kind, &self.columns, &seek.values, render);
                if segments.is_empty() { Start::Empty } else { Start::Seek(segments) }
            }
            _ => Start::Offset(cursor.rows_before),
        }
    }

    /// Finishes a page fetched with `limit + 1`: drops the extra row and, if there was one, makes
    /// the cursor for the next page from the last kept row's `keys` (one entry per fetched row,
    /// values in [`Self::key_indexes`] order). Rows are also cut back to `width` values, dropping
    /// selected row ids.
    pub fn finish(
        &self,
        mut result: QueryResult,
        keys: Vec<Vec<CursorValue>>,
        width: usize,
        limit: u32,
        after: Option<&PageCursor>,
    ) -> RowPage {
        let limit = limit as usize;
        let before = after.filter(|c| c.fingerprint == self.fingerprint).map_or(0, |c| c.rows_before);
        let more = result.rows.len() > limit;
        result.rows.truncate(limit);
        for row in &mut result.rows {
            row.truncate(width);
        }
        let next = more.then(|| PageCursor {
            fingerprint: self.fingerprint,
            rows_before: before + limit as u64,
            seek: self
                .enabled
                .then(|| keys.into_iter().nth(limit - 1))
                .flatten()
                .filter(|values| values.len() == self.columns.len())
                .map(|values| Seek { layout: self.layout, values }),
        });
        RowPage { result, next }
    }
}

/// The rows after `values` in the order of `columns`, as conditions to query in turn (empty: no
/// row comes after them). A nullable first term splits into its NULL and non-NULL parts, so each
/// part can seek an index on the sort columns instead of OR-ing `is null` into the range.
pub fn seek_segments(
    kind: DatabaseKind,
    columns: &[SeekColumn],
    values: &[CursorValue],
    mut render: impl FnMut(usize, &CursorValue) -> String,
) -> Vec<String> {
    let first = &columns[0];
    if !first.nullable && !values[0].is_null() {
        return seek_predicate(kind, columns, values, render).into_iter().collect();
    }
    let e = &first.expr;
    let nulls_last = NullOrder::of(kind).last(first.descending);
    let mut segments = Vec::new();
    if values[0].is_null() {
        // Still among the NULLs: the rest of them, then (if NULLs sort first) every non-NULL row.
        if columns.len() > 1 {
            if let Some(rest) = seek_predicate(kind, &columns[1..], &values[1..], |i, v| render(i + 1, v)) {
                segments.push(format!("{e} is null and ({rest})"));
            }
        }
        if !nulls_last {
            segments.push(format!("{e} is not null"));
        }
    } else {
        // Among the values: the comparison already leaves NULLs out; they follow if they sort last.
        let mut non_null = columns.to_vec();
        non_null[0].nullable = false;
        segments.extend(seek_predicate(kind, &non_null, values, &mut render));
        if nulls_last {
            segments.push(format!("{e} is null"));
        }
    }
    segments
}

/// The condition for rows strictly after `values` in the order of `columns`, or `None` when no
/// row can come after them. `render` is called for non-NULL values only.
pub fn seek_predicate(
    kind: DatabaseKind,
    columns: &[SeekColumn],
    values: &[CursorValue],
    mut render: impl FnMut(usize, &CursorValue) -> String,
) -> Option<String> {
    assert_eq!(columns.len(), values.len());
    let nulls = NullOrder::of(kind);
    // A NULL in a column said to be NOT NULL (SQLite allows that in some keys) is still a NULL.
    let nullable = |i: usize| columns[i].nullable || values[i].is_null();

    // Same direction and no NULL values: one row-value comparison, which indexes handle best.
    // `(a, b) > (x, y)` means `a > x or (a = x and b > y)`, so a NULL `b` is left out when
    // `a = x`: right only where NULLs sort before `y`.
    let descending = columns.first().is_some_and(|c| c.descending);
    if supports_row_values(kind)
        && columns.iter().all(|c| c.descending == descending)
        && values.iter().all(|v| !v.is_null())
        && columns.iter().all(|c| !c.nullable || !nulls.last(c.descending))
    {
        let op = if descending { "<" } else { ">" };
        if columns.len() == 1 {
            return Some(format!("{} {op} {}", columns[0].expr, render(0, &values[0])));
        }
        let lhs: Vec<&str> = columns.iter().map(|c| c.expr.as_str()).collect();
        let rhs: Vec<String> = values.iter().enumerate().map(|(i, v)| render(i, v)).collect();
        return Some(format!("({}) {op} ({})", lhs.join(", "), rhs.join(", ")));
    }

    // Expanded form: the first i-1 terms equal and the i-th one after, for some i.
    let mut disjuncts = Vec::new();
    let mut equal: Vec<String> = Vec::new();
    for (i, (column, value)) in columns.iter().zip(values).enumerate() {
        let e = &column.expr;
        let nulls_last = nulls.last(column.descending);
        let after = if value.is_null() {
            // After NULL come the non-NULL values when NULLs sort first; nothing when they sort last.
            (!nulls_last).then(|| format!("{e} is not null"))
        } else {
            let op = if column.descending { "<" } else { ">" };
            let base = format!("{e} {op} {}", render(i, value));
            Some(if nulls_last && nullable(i) { format!("({base} or {e} is null)") } else { base })
        };
        if let Some(after) = after {
            let mut terms = equal.clone();
            terms.push(after);
            disjuncts.push(terms.join(" and "));
        }
        if i + 1 == columns.len() {
            break;
        }
        equal.push(if value.is_null() { format!("{e} is null") } else { format!("{e} = {}", render(i, value)) });
    }
    match disjuncts.len() {
        0 => None,
        1 => disjuncts.pop(),
        _ => {
            let body = disjuncts.iter().map(|d| format!("({d})")).collect::<Vec<_>>().join(" or ");
            // A redundant bound on the first term gives the planner an index range.
            let first = &columns[0];
            let e = &first.expr;
            let lead = if values[0].is_null() {
                nulls.last(first.descending).then(|| format!("{e} is null"))
            } else {
                let op = if first.descending { "<=" } else { ">=" };
                let bound = format!("{e} {op} {}", render(0, &values[0]));
                Some(if nulls.last(first.descending) && nullable(0) { format!("({bound} or {e} is null)") } else { bound })
            };
            Some(match lead {
                Some(lead) => format!("{lead} and ({body})"),
                None => body,
            })
        }
    }
}

/// `select *[, extra…] from rel [where (filter) and (seek)] [order by …] limit n [offset m]`.
/// `filter` must come from [`crate::dialect::normalize_filter`].
pub fn page_sql(
    relation: &str,
    extra: &[String],
    filter: Option<&str>,
    seek: Option<&str>,
    order_by: &[String],
    limit: u64,
    offset: u64,
) -> String {
    let mut select = String::from("*");
    for e in extra {
        select.push_str(", ");
        select.push_str(e);
    }
    page_sql_with(&select, relation, filter, seek, order_by, limit, offset)
}

/// [`page_sql`] with its own select list (`select <select> from rel …`).
pub fn page_sql_with(
    select: &str,
    relation: &str,
    filter: Option<&str>,
    seek: Option<&str>,
    order_by: &[String],
    limit: u64,
    offset: u64,
) -> String {
    let mut sql = format!("select {select}");
    sql.push_str(" from ");
    sql.push_str(relation);
    // The filter goes on its own lines so a trailing `-- comment` can't swallow the closing paren.
    match (filter, seek) {
        (Some(f), Some(s)) => sql.push_str(&format!(" where (\n{f}\n) and ({s})")),
        (Some(f), None) => sql.push_str(&format!(" where (\n{f}\n)")),
        (None, Some(s)) => sql.push_str(&format!(" where {s}")),
        (None, None) => {}
    }
    if !order_by.is_empty() {
        sql.push_str(&format!(" order by {}", order_by.join(", ")));
    }
    sql.push_str(&format!(" limit {limit}"));
    if offset > 0 {
        sql.push_str(&format!(" offset {offset}"));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(expr: &str, descending: bool, nullable: bool) -> SeekColumn {
        SeekColumn { expr: expr.into(), descending, nullable, index: 0 }
    }

    fn lit(_: usize, v: &CursorValue) -> String {
        match v {
            CursorValue::Int(i) => i.to_string(),
            CursorValue::Text(t) => format!("'{t}'"),
            other => format!("{other:?}"),
        }
    }

    const PG: DatabaseKind = DatabaseKind::Postgres;
    const MY: DatabaseKind = DatabaseKind::Mysql;
    const SL: DatabaseKind = DatabaseKind::Sqlite;

    #[test]
    fn uses_row_values_when_possible() {
        let id = [col("id", false, false)];
        assert_eq!(seek_predicate(PG, &id, &[CursorValue::Int(5)], lit).unwrap(), "id > 5");
        let pk = [col("a", true, false), col("b", true, false)];
        let v = [CursorValue::Int(1), CursorValue::Text("x".into())];
        assert_eq!(seek_predicate(SL, &pk, &v, lit).unwrap(), "(a, b) < (1, 'x')");
        // MySQL wouldn't use an index for that.
        assert_eq!(seek_predicate(MY, &pk, &v, lit).unwrap(), "a <= 1 and ((a < 1) or (a = 1 and b < 'x'))");
    }

    #[test]
    fn expands_mixed_directions() {
        let cols = [col("a", true, false), col("id", false, false)];
        let v = [CursorValue::Int(3), CursorValue::Int(9)];
        assert_eq!(seek_predicate(PG, &cols, &v, lit).unwrap(), "a <= 3 and ((a < 3) or (a = 3 and id > 9))");
    }

    #[test]
    fn handles_nulls_per_engine() {
        let cols = [col("a", false, true), col("id", false, false)];
        let v = [CursorValue::Int(3), CursorValue::Int(9)];
        // Postgres: NULLs come after 3 ascending.
        assert_eq!(
            seek_predicate(PG, &cols, &v, lit).unwrap(),
            "(a >= 3 or a is null) and (((a > 3 or a is null)) or (a = 3 and id > 9))"
        );
        // SQLite: NULLs came before 3, so the row value is right.
        assert_eq!(seek_predicate(SL, &cols, &v, lit).unwrap(), "(a, id) > (3, 9)");
        assert_eq!(seek_predicate(MY, &cols, &v, lit).unwrap(), "a >= 3 and ((a > 3) or (a = 3 and id > 9))");

        let at_null = [CursorValue::Null, CursorValue::Int(9)];
        assert_eq!(seek_predicate(PG, &cols, &at_null, lit).unwrap(), "a is null and id > 9");
        assert_eq!(seek_predicate(MY, &cols, &at_null, lit).unwrap(), "(a is not null) or (a is null and id > 9)");
        // Last NULL of a nullable last term (no tiebreak) on Postgres: nothing after it.
        assert_eq!(seek_predicate(PG, &[col("a", false, true)], &[CursorValue::Null], lit), None);
    }

    #[test]
    fn splits_nullable_first_terms() {
        let cols = [col("a", false, true), col("id", false, false)];
        let v = [CursorValue::Int(3), CursorValue::Int(9)];
        assert_eq!(seek_segments(PG, &cols, &v, lit), ["(a, id) > (3, 9)", "a is null"]);
        assert_eq!(seek_segments(SL, &cols, &v, lit), ["(a, id) > (3, 9)"]);
        let at_null = [CursorValue::Null, CursorValue::Int(9)];
        assert_eq!(seek_segments(PG, &cols, &at_null, lit), ["a is null and (id > 9)"]);
        assert_eq!(seek_segments(MY, &cols, &at_null, lit), ["a is null and (id > 9)", "a is not null"]);
        let desc = [col("a", true, true), col("id", false, false)];
        assert_eq!(seek_segments(PG, &desc, &v, lit), ["a <= 3 and ((a < 3) or (a = 3 and id > 9))"]);
    }

    #[test]
    fn builds_page_sql() {
        assert_eq!(page_sql("t", &[], None, None, &[], 10, 0), "select * from t limit 10");
        assert_eq!(
            page_sql("t", &["ctid".into()], Some("a = 1 -- x"), Some("id > 5"), &["id".into()], 11, 0),
            "select *, ctid from t where (\na = 1 -- x\n) and (id > 5) order by id limit 11"
        );
        assert_eq!(page_sql("t", &[], None, Some("id > 5"), &["id".into()], 3, 20), "select * from t where id > 5 order by id limit 3 offset 20");
    }

    #[test]
    fn cursors_round_trip_and_check_their_query() {
        let table = TableInfo::new("public", "t");
        let query = RowQuery::default();
        let columns = [ColumnInfo { name: "id".into(), type_name: "int".into(), is_primary_key: true, is_nullable: false }];
        let d = Dialect(PG);
        let tiebreak = vec![SeekColumn::column(d, &columns, "id").unwrap()];
        let keyset = Keyset::new(d, &table, &query, &columns, tiebreak.clone(), true).unwrap();
        let rows = |n: i64| QueryResult { rows: (0..n).map(|i| vec![crate::Value::Int(i)]).collect(), ..Default::default() };
        let keys = |n: i64| (0..n).map(|i| vec![CursorValue::Int(i)]).collect();

        let page = keyset.finish(rows(4), keys(4), 1, 3, None);
        assert_eq!(page.result.rows.len(), 3);
        let next = page.next.unwrap();
        assert_eq!(next.rows_before(), 3);
        let next = PageCursor::decode(&next.encode()).unwrap();
        assert_eq!(keyset.start(Some(&next), lit), Start::Seek(vec!["\"id\" > 2".into()]));
        assert!(keyset.finish(rows(2), keys(2), 1, 3, Some(&next)).next.is_none());

        // Another sort: start over. Same query but keyset off: OFFSET.
        let other = RowQuery { sort: vec![SortKey { column: "id".into(), descending: true }], filter: None };
        let sorted = Keyset::new(d, &table, &other, &columns, tiebreak.clone(), true).unwrap();
        assert_eq!(sorted.start(Some(&next), lit), Start::Offset(0));
        let off = Keyset::new(d, &table, &query, &columns, tiebreak, false).unwrap();
        assert_eq!(off.start(Some(&next), lit), Start::Offset(3));
        assert!(!off.finish(rows(4), keys(4), 1, 3, None).next.unwrap().is_keyset());

        // Infinite floats survive the JSON round trip.
        let c = PageCursor { fingerprint: 1, rows_before: 2, seek: Some(Seek { layout: 3, values: vec![CursorValue::float(f64::INFINITY), CursorValue::Bytes(vec![0, 255])] }) };
        assert_eq!(PageCursor::decode(&c.encode()).unwrap(), c);
        assert!(PageCursor::decode("nope").is_err());
    }

    #[test]
    fn rejects_unknown_sort_columns() {
        let columns = [ColumnInfo { name: "id".into(), type_name: String::new(), is_primary_key: true, is_nullable: false }];
        let query = RowQuery { sort: vec![SortKey { column: "nope".into(), descending: false }], filter: None };
        assert!(Keyset::new(Dialect(MY), &TableInfo::new("s", "t"), &query, &columns, vec![], true).is_err());
    }

    /// Exhaustive check of the predicate against SQLite, for both NULL orders: every 1–3 term sort
    /// over columns with duplicates and NULLs, paged with every page size, must give the same rows
    /// as one ordered query. `NullOrder::Largest` is checked with explicit `nulls last/first`.
    #[test]
    fn seeks_match_full_order_in_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table t (id integer primary key, a int, b text, c real);
             insert into t (a, b, c) values
               (1, 'x', 1.5), (null, 'x', null), (1, null, 2.5), (2, 'y', 1.5), (null, null, null),
               (2, 'x', 1.5), (1, 'X', -1.0), (3, 'y', null), (null, 'y', 2.5), (1, 'x', 1.5),
               (2, null, 0.0), (3, 'x', 1.5), (null, 'x', 1.5);",
        )
        .unwrap();
        let rows: Vec<Vec<rusqlite::types::Value>> = conn
            .prepare("select id, a, b, c from t")
            .unwrap()
            .query_map([], |r| (0..4).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect())
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let names = ["id", "a", "b", "c"];
        let key = |v: &rusqlite::types::Value| match v {
            rusqlite::types::Value::Null => CursorValue::Null,
            rusqlite::types::Value::Integer(i) => CursorValue::Int(*i),
            rusqlite::types::Value::Real(f) => CursorValue::float(*f),
            rusqlite::types::Value::Text(t) => CursorValue::Text(t.clone()),
            rusqlite::types::Value::Blob(b) => CursorValue::Bytes(b.clone()),
        };
        let back = |v: &CursorValue| match v {
            CursorValue::Null => rusqlite::types::Value::Null,
            CursorValue::Int(i) => rusqlite::types::Value::Integer(*i),
            CursorValue::Float(f) => rusqlite::types::Value::Real(f64::from_bits(*f)),
            CursorValue::Text(t) => rusqlite::types::Value::Text(t.clone()),
            CursorValue::Bytes(b) => rusqlite::types::Value::Blob(b.clone()),
        };

        let mut sorts: Vec<Vec<(usize, bool)>> = vec![vec![]];
        for x in 1..4 {
            for dx in [false, true] {
                sorts.push(vec![(x, dx)]);
                for y in 1..4 {
                    for dy in [false, true] {
                        if y != x {
                            sorts.push(vec![(x, dx), (y, dy)]);
                            for z in 1..4 {
                                if z != x && z != y {
                                    sorts.push(vec![(x, dx), (y, dy), (z, !dy)]);
                                }
                            }
                        }
                    }
                }
            }
        }
        // MySQL: NULLs first like SQLite, but always the expanded form (no row values).
        for kind in [DatabaseKind::Postgres, DatabaseKind::Sqlite, DatabaseKind::Mysql] {
            let nulls = NullOrder::of(kind);
            for sort in &sorts {
                let mut columns: Vec<SeekColumn> =
                    sort.iter().map(|&(i, d)| SeekColumn { expr: names[i].into(), descending: d, nullable: true, index: i }).collect();
                columns.push(SeekColumn { expr: "id".into(), descending: false, nullable: false, index: 0 });
                let order: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        let dir = if c.descending { "desc" } else { "asc" };
                        let n = if nulls.last(c.descending) { "nulls last" } else { "nulls first" };
                        format!("{} {dir} {n}", c.expr)
                    })
                    .collect();
                let fetch = |seek: Option<(String, Vec<rusqlite::types::Value>)>, limit: usize| -> Vec<Vec<rusqlite::types::Value>> {
                    let (w, params) = seek.map_or((String::new(), vec![]), |(s, p)| (format!(" where {s}"), p));
                    let sql = format!("select id, a, b, c from t{w} order by {} limit {limit}", order.join(", "));
                    let mut stmt = conn.prepare(&sql).unwrap();
                    let params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                    stmt.query_map(params.as_slice(), |r| (0..4).map(|i| r.get(i)).collect()).unwrap().collect::<rusqlite::Result<_>>().unwrap()
                };
                let all = fetch(None, 1000);
                assert_eq!(all.len(), rows.len());
                for limit in 1..5 {
                    let mut got = fetch(None, limit);
                    while let Some(last) = got.last().cloned().filter(|_| got.len() % limit == 0) {
                        let values: Vec<CursorValue> = columns.iter().map(|c| key(&last[c.index])).collect();
                        let mut params = vec![rusqlite::types::Value::Null; columns.len()];
                        let segments = seek_segments(kind, &columns, &values, |i, v| {
                            params[i] = back(v);
                            format!("?{}", i + 1)
                        });
                        let before = got.len();
                        for segment in segments {
                            let need = limit - (got.len() - before);
                            if need == 0 {
                                break;
                            }
                            // Bind only up to the highest `?N` the segment uses.
                            let used = (1..=params.len()).rev().find(|n| segment.contains(&format!("?{n}"))).unwrap_or(0);
                            got.extend(fetch(Some((segment, params[..used].to_vec())), need));
                        }
                        if got.len() == before {
                            break;
                        }
                    }
                    assert_eq!(got, all, "{kind:?} sort {sort:?} limit {limit}");
                }
            }
        }
    }
}
