//! Per-database SQL spelling, shared by the drivers and by features that generate SQL
//! (sorting, filters, editing…), so those are written once for every backend.

use crate::driver::{Error, Result};
use crate::model::{ColumnInfo, DatabaseKind, SortKey, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect(pub DatabaseKind);

impl Dialect {
    /// Quotes an identifier: `"name"` (Postgres, SQLite), `` `name` `` (MySQL) or `[name]` (SQL Server).
    pub fn quote_ident(self, name: &str) -> String {
        match self.0 {
            DatabaseKind::Mysql => format!("`{}`", name.replace('`', "``")),
            DatabaseKind::Postgres | DatabaseKind::Sqlite | DatabaseKind::Libsql => format!("\"{}\"", name.replace('"', "\"\"")),
            DatabaseKind::SqlServer => format!("[{}]", name.replace(']', "]]")),
        }
    }

    /// `schema.table`, both quoted. For MySQL the schema is the database; for SQLite the attached database name.
    pub fn quote_relation(self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }

    /// `create database <name>`. Databases are server-level, so SQLite and libSQL have none to create.
    pub fn create_database(self, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::InvalidConfig("Enter a database name.".into()));
        }
        if name.contains('\0') {
            return Err(Error::InvalidConfig("Database names can’t contain NUL characters.".into()));
        }
        match self.0 {
            DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::SqlServer => {
                Ok(format!("create database {}", self.quote_ident(name)))
            }
            DatabaseKind::Sqlite | DatabaseKind::Libsql => {
                Err(Error::Unsupported(format!("{} has no databases to create", self.0.display_name())))
            }
        }
    }

    /// A string literal, e.g. for catalog queries that can't take parameters.
    pub fn quote_literal(self, value: &str) -> String {
        match self.0 {
            // Backslash is an escape character in MySQL strings (unless NO_BACKSLASH_ESCAPES).
            DatabaseKind::Mysql => format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''")),
            DatabaseKind::Postgres | DatabaseKind::Sqlite | DatabaseKind::Libsql => format!("'{}'", value.replace('\'', "''")),
            // `N'…'`: Unicode, so non-Latin text survives into NVARCHAR columns.
            DatabaseKind::SqlServer => format!("N'{}'", value.replace('\'', "''")),
        }
    }

    /// One page of rows: `select * from rel [where (…)] [order by …] limit n offset m`. SQL Server spells
    /// it `order by … offset m rows fetch next n rows only`, which needs an `order by` (`(select null)`
    /// when there's nothing to sort by). `filter` must come from [`normalize_filter`].
    pub fn page_query(self, relation: &str, filter: Option<&str>, order_by: &[String], limit: u32, offset: u64) -> String {
        let order = if order_by.is_empty() { String::new() } else { format!(" order by {}", order_by.join(", ")) };
        if self.0 == DatabaseKind::SqlServer {
            let order = if order_by.is_empty() { " order by (select null)".to_string() } else { order };
            return format!(
                "select * from {relation}{}{order} offset {offset} rows fetch next {limit} rows only",
                where_clause(filter)
            );
        }
        format!("select * from {relation}{}{order} limit {limit} offset {offset}", where_clause(filter))
    }

    /// `"a" = 1 and "b" = 'x'`: the rows whose `columns` hold `values`, e.g. the row a foreign key
    /// points at. Numbers go in bare; text goes in as a string literal the database casts to the
    /// column's type (uuid, date…). Pairs past the shorter of the two lists are ignored.
    pub fn match_filter(self, columns: &[String], values: &[Value]) -> String {
        let terms: Vec<String> = columns
            .iter()
            .zip(values)
            .map(|(column, value)| {
                let ident = self.quote_ident(column);
                match value {
                    Value::Null => format!("{ident} is null"),
                    value => format!("{ident} = {}", self.literal(value)),
                }
            })
            .collect();
        terms.join(" and ")
    }

    /// A value as a SQL literal (not `NULL`, which needs `is null` in a comparison).
    pub(crate) fn literal(self, value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(b) if self.0 == DatabaseKind::Postgres => b.to_string(),
            Value::Bool(b) => (if *b { "1" } else { "0" }).into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Decimal(d) if is_plain_number(d) => d.clone(),
            Value::Float(_) | Value::Decimal(_) => self.quote_literal(&value.display()),
            Value::Text(t) => self.quote_literal(t),
        }
    }

    /// `select count(*) from rel [where (…)]`.
    pub fn count_query(self, relation: &str, filter: Option<&str>) -> String {
        format!("select count(*) from {relation}{}", where_clause(filter))
    }

    /// `ORDER BY` terms: the user's sort, then the `tiebreak` terms (already quoted, e.g. the
    /// primary key) that aren't sorted on yet, so pages never overlap or skip rows.
    /// Fails on columns the table doesn't have.
    pub fn order_by(self, sort: &[SortKey], columns: &[ColumnInfo], tiebreak: &[String]) -> Result<Vec<String>> {
        let mut terms = Vec::with_capacity(sort.len() + tiebreak.len());
        let mut used = Vec::new();
        for key in sort {
            if !columns.iter().any(|c| c.name == key.column) {
                return Err(Error::Query(format!("Can’t sort by “{}”: no such column", key.column)));
            }
            let ident = self.quote_ident(&key.column);
            if used.contains(&ident) {
                continue;
            }
            terms.push(if key.descending { format!("{ident} desc") } else { ident.clone() });
            used.push(ident);
        }
        terms.extend(tiebreak.iter().filter(|t| !used.contains(t)).cloned());
        Ok(terms)
    }
}

/// `-12.50`, `1e5`: safe to put in SQL without quotes.
fn is_plain_number(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit()) && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
}

/// The filter goes on its own lines so a trailing `-- comment` can't swallow the closing paren.
fn where_clause(filter: Option<&str>) -> String {
    filter.map_or(String::new(), |f| format!(" where (\n{f}\n)"))
}

/// Whether running `sql` may have changed the list of tables (`create`, `drop`, `alter`, `rename`),
/// so a frontend should list them again. A keyword in a comment or string also counts: that costs
/// one extra listing, which is cheaper than parsing every dialect.
pub fn changes_schema(sql: &str) -> bool {
    static DDL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\b(create|drop|alter|rename)\b").unwrap());
    DDL.is_match(sql)
}

/// Cleans up a user `WHERE` filter: trims it, drops a leading `where` and trailing `;`s, and
/// returns `None` when nothing is left. Rejects a `;` between statements, so a filter can't
/// smuggle in a second statement (quotes and comments are skipped when looking for one).
pub fn normalize_filter(filter: Option<&str>) -> Result<Option<String>> {
    let Some(filter) = filter else { return Ok(None) };
    let mut f = filter.trim();
    if f.len() >= 6 && f[..5].eq_ignore_ascii_case("where") && f[5..].starts_with(char::is_whitespace) {
        f = f[5..].trim_start();
    }
    let f = f.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    if f.is_empty() {
        return Ok(None);
    }
    if has_statement_separator(f) {
        return Err(Error::Query("A filter is a single condition, like `status = 'paid'`: remove the “;”.".into()));
    }
    Ok(Some(f.to_string()))
}

/// Whether `sql` has a `;` outside string literals, quoted identifiers and comments.
///
/// Lexes the union of the dialects' rules, MySQL's included (`#` comments, `\'` escapes): MySQL runs
/// browse queries through its multi-statement text protocol, so this is its only guard. Postgres and
/// SQLite additionally prepare exactly one statement, so reading their SQL loosely here is harmless.
fn has_statement_separator(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ';' => return true,
            '\'' | '"' | '`' => {
                // Doubled quotes (`''`) just end and restart the literal, which this handles too.
                while let Some(d) = chars.next() {
                    if d == '\\' && c != '`' {
                        chars.next();
                    } else if d == c {
                        break;
                    }
                }
            }
            '#' => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            // MySQL needs whitespace after `--` (`1--1` is arithmetic there); requiring it everywhere
            // only makes Postgres/SQLite `--x;` comments look like separators, which is the safe side.
            '-' if chars.peek() == Some(&'-') && chars.clone().nth(1).is_none_or(char::is_whitespace) => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            _ => {}
        }
    }
    false
}

/// Binary values shown in the grid: `0x0a1b…`, cut after `MAX_BLOB_PREVIEW` bytes.
pub(crate) fn hex_preview(bytes: &[u8]) -> String {
    const MAX_BLOB_PREVIEW: usize = 4096;
    let shown = &bytes[..bytes.len().min(MAX_BLOB_PREVIEW)];
    let mut out = String::with_capacity(2 + shown.len() * 2 + 1);
    out.push_str("0x");
    for b in shown {
        out.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > MAX_BLOB_PREVIEW {
        out.push('…');
    }
    out
}

/// An error with its sources: "error connecting to server: Connection refused (os error 61)".
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = s.source();
    }
    message
}

/// 1-based line and column of a character offset (0-based) in `sql`.
pub(crate) fn line_column(sql: &str, char_offset: usize) -> (usize, usize) {
    let before: String = sql.chars().take(char_offset).collect();
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_schema_spots_ddl() {
        assert!(changes_schema("CREATE TABLE t (id int)"));
        assert!(changes_schema("select 1;\ndrop view v"));
        assert!(changes_schema("alter table t add column x int"));
        assert!(!changes_schema("select created_at, dropped from t"));
        assert!(!changes_schema("update t set x = 1"));
    }

    #[test]
    fn create_database_quotes_the_name() {
        assert_eq!(Dialect(DatabaseKind::Postgres).create_database(" My \"db\" ").unwrap(), r#"create database "My ""db""""#);
        assert_eq!(Dialect(DatabaseKind::Mysql).create_database("shop`x").unwrap(), "create database `shop``x`");
        assert_eq!(Dialect(DatabaseKind::SqlServer).create_database("a]b").unwrap(), "create database [a]]b]");
        assert!(matches!(Dialect(DatabaseKind::Postgres).create_database("  "), Err(Error::InvalidConfig(_))));
        assert!(matches!(Dialect(DatabaseKind::Sqlite).create_database("x"), Err(Error::Unsupported(_))));
    }

    #[test]
    fn quotes_per_dialect() {
        let pg = Dialect(DatabaseKind::Postgres);
        let my = Dialect(DatabaseKind::Mysql);
        assert_eq!(pg.quote_relation("public", r#"we"ird"#), r#""public"."we""ird""#);
        assert_eq!(my.quote_relation("shop", "we`ird"), "`shop`.`we``ird`");
        assert_eq!(my.quote_literal(r"it's a\b"), r"'it''s a\\b'");
        assert_eq!(pg.quote_literal(r"it's a\b"), r"'it''s a\b'");
    }

    #[test]
    fn match_filter_per_dialect() {
        let cols = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pg = Dialect(DatabaseKind::Postgres);
        assert_eq!(pg.match_filter(&cols(&["id"]), &[Value::Int(42)]), r#""id" = 42"#);
        assert_eq!(
            pg.match_filter(&cols(&["org", "code"]), &[Value::Text("o'k".into()), Value::Bool(true)]),
            r#""org" = 'o''k' and "code" = true"#
        );
        assert_eq!(pg.match_filter(&cols(&["a"]), &[Value::Null]), r#""a" is null"#);
        assert_eq!(pg.match_filter(&cols(&["a"]), &[Value::Decimal("-1.50".into())]), r#""a" = -1.50"#);
        assert_eq!(pg.match_filter(&cols(&["a"]), &[Value::Decimal("NaN".into())]), r#""a" = 'NaN'"#);
        let my = Dialect(DatabaseKind::Mysql);
        assert_eq!(my.match_filter(&cols(&["flag"]), &[Value::Bool(true)]), "`flag` = 1");
        let ms = Dialect(DatabaseKind::SqlServer);
        assert_eq!(ms.match_filter(&cols(&["id"]), &[Value::Text("a-b".into())]), "[id] = N'a-b'");
        // The result is a valid single-condition filter.
        assert!(normalize_filter(Some(&pg.match_filter(&cols(&["x"]), &[Value::Text("a;b".into())]))).is_ok());
    }

    #[test]
    fn quotes_sql_server() {
        let ms = Dialect(DatabaseKind::SqlServer);
        assert_eq!(ms.quote_relation("dbo", "we]ird name"), "[dbo].[we]]ird name]");
        assert_eq!(ms.quote_literal("it's ☃"), "N'it''s ☃'");
        assert_eq!(
            ms.page_query("[dbo].[t]", Some("a = 1"), &["[id]".into()], 50, 100),
            "select * from [dbo].[t] where (\na = 1\n) order by [id] offset 100 rows fetch next 50 rows only"
        );
        assert_eq!(
            ms.page_query("[dbo].[t]", None, &[], 50, 0),
            "select * from [dbo].[t] order by (select null) offset 0 rows fetch next 50 rows only"
        );
    }

    #[test]
    fn builds_page_queries() {
        let d = Dialect(DatabaseKind::Sqlite);
        assert_eq!(d.page_query("\"main\".\"t\"", None, &[], 10, 0), "select * from \"main\".\"t\" limit 10 offset 0");
        assert_eq!(d.page_query("t", None, &["\"id\"".into()], 5, 20), "select * from t order by \"id\" limit 5 offset 20");
        assert_eq!(
            d.page_query("t", Some("a = 1 -- note"), &[], 5, 0),
            "select * from t where (\na = 1 -- note\n) limit 5 offset 0"
        );
        assert_eq!(d.count_query("t", Some("a = 1")), "select count(*) from t where (\na = 1\n)");
    }

    #[test]
    fn orders_by_sort_then_tiebreak() {
        let d = Dialect(DatabaseKind::Mysql);
        let columns = [
            ColumnInfo { name: "id".into(), type_name: String::new(), is_primary_key: true, is_nullable: false },
            ColumnInfo { name: "total".into(), type_name: String::new(), is_primary_key: false, is_nullable: false },
        ];
        let sort = |c: &str, descending| SortKey { column: c.into(), descending };
        let tiebreak = ["`id`".to_string()];
        assert_eq!(d.order_by(&[], &columns, &tiebreak).unwrap(), ["`id`"]);
        assert_eq!(d.order_by(&[sort("total", true)], &columns, &tiebreak).unwrap(), ["`total` desc", "`id`"]);
        assert_eq!(d.order_by(&[sort("id", true)], &columns, &tiebreak).unwrap(), ["`id` desc"]);
        assert!(d.order_by(&[sort("nope", false)], &columns, &tiebreak).is_err());
    }

    #[test]
    fn normalizes_filters() {
        assert_eq!(normalize_filter(None).unwrap(), None);
        assert_eq!(normalize_filter(Some("  ; ")).unwrap(), None);
        assert_eq!(normalize_filter(Some("WHERE id > 3;")).unwrap().as_deref(), Some("id > 3"));
        assert_eq!(normalize_filter(Some("whereabouts = 1")).unwrap().as_deref(), Some("whereabouts = 1"));
        assert_eq!(normalize_filter(Some("note = 'a;b' -- x;\n")).unwrap().as_deref(), Some("note = 'a;b' -- x"));
        assert_eq!(normalize_filter(Some("\"we;ird\" /* ; */ = 1")).unwrap().as_deref(), Some("\"we;ird\" /* ; */ = 1"));
        assert!(normalize_filter(Some("1 = 1; drop table users")).is_err());
        // MySQL: a backslash-escaped quote doesn't end the string, `#` starts a comment.
        assert!(normalize_filter(Some(r"a = '\'' ; drop table t; '")).is_err());
        assert!(normalize_filter(Some("a = 1 # '\n; drop table t; -- '")).is_err());
        assert!(normalize_filter(Some("a = 1--1; drop table t")).is_err());
    }

    #[test]
    fn previews_blobs() {
        assert_eq!(hex_preview(&[0, 0xab, 0xff]), "0x00abff");
        assert!(hex_preview(&vec![1; 5000]).ends_with('…'));
    }

    #[test]
    fn maps_offsets_to_line_and_column() {
        assert_eq!(line_column("select\n  fro", 9), (2, 3));
        assert_eq!(line_column("selec 1", 0), (1, 1));
    }
}
