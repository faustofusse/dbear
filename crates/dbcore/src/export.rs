//! Turning grid rows into text for the clipboard (TSV, CSV, JSON, Markdown, `INSERT`s), and
//! formatting single values for the value inspector. Shared so every frontend copies the same way.

use crate::dialect::Dialect;
use crate::edit::is_binary;
use crate::model::{ColumnInfo, DatabaseKind, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CopyFormat {
    /// Tab-separated, what spreadsheets paste into cells. NULL is an empty field.
    Tsv,
    /// RFC 4180 CSV. NULL is an empty field.
    Csv,
    /// An array of objects, one per row, keys in column order.
    Json,
    /// A GitHub-flavored Markdown table (always with a header).
    Markdown,
    /// One `INSERT` statement per row.
    Insert,
}

/// Where the rows come from: spelled into `INSERT`s (`None` for script results).
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub kind: DatabaseKind,
    pub schema: Option<&'a str>,
    pub table: Option<&'a str>,
}

/// `rows` as text in `format`. `headers` adds a header line to TSV and CSV (Markdown always has
/// one, JSON and `INSERT` name the columns anyway). Rows shorter than `columns` read as NULL.
pub fn format_rows(format: CopyFormat, target: Target, columns: &[ColumnInfo], rows: &[Vec<Value>], headers: bool) -> String {
    let cell = |row: &[Value], i: usize| row.get(i).cloned().unwrap_or(Value::Null);
    match format {
        CopyFormat::Tsv | CopyFormat::Csv => {
            let separator = if format == CopyFormat::Tsv { '\t' } else { ',' };
            let field = |s: &str| delimited_field(s, separator);
            let mut lines = Vec::with_capacity(rows.len() + 1);
            if headers {
                lines.push(columns.iter().map(|c| field(&c.name)).collect::<Vec<_>>().join(&separator.to_string()));
            }
            for row in rows {
                let fields: Vec<String> = (0..columns.len())
                    .map(|i| match cell(row, i) {
                        Value::Null => String::new(),
                        v => field(&v.display()),
                    })
                    .collect();
                lines.push(fields.join(&separator.to_string()));
            }
            // CSV lines end in CRLF per RFC 4180; spreadsheets read either.
            lines.join(if format == CopyFormat::Csv { "\r\n" } else { "\n" })
        }
        CopyFormat::Json => {
            let keys = unique_names(columns);
            let mut out = String::from("[");
            for (r, row) in rows.iter().enumerate() {
                out.push_str(if r == 0 { "\n  {" } else { ",\n  {" });
                for (i, column) in columns.iter().enumerate() {
                    out.push_str(if i == 0 { "\n    " } else { ",\n    " });
                    out.push_str(&json_string(&keys[i]));
                    out.push_str(": ");
                    out.push_str(&json_value(column, &cell(row, i)));
                }
                out.push_str(if columns.is_empty() { "}" } else { "\n  }" });
            }
            out.push_str(if rows.is_empty() { "]" } else { "\n]" });
            out
        }
        CopyFormat::Markdown => {
            let escape = |s: &str| s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace('\n', "<br>");
            let mut lines = vec![
                format!("| {} |", columns.iter().map(|c| escape(&c.name)).collect::<Vec<_>>().join(" | ")),
                format!("|{}|", columns.iter().map(|_| " --- ").collect::<Vec<_>>().join("|")),
            ];
            for row in rows {
                let fields: Vec<String> = (0..columns.len()).map(|i| escape(&cell(row, i).display())).collect();
                lines.push(format!("| {} |", fields.join(" | ")));
            }
            lines.join("\n")
        }
        CopyFormat::Insert => {
            let d = Dialect(target.kind);
            let table = match (target.schema, target.table) {
                (Some(schema), Some(table)) if !schema.is_empty() => d.quote_relation(schema, table),
                (_, Some(table)) => d.quote_ident(table),
                // Script results: a placeholder to fill in.
                _ => d.quote_ident("table_name"),
            };
            let names = columns.iter().map(|c| d.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
            rows.iter()
                .map(|row| {
                    let values: Vec<String> = columns.iter().enumerate().map(|(i, c)| sql_literal(d, c, &cell(row, i))).collect();
                    format!("insert into {table} ({names}) values ({});", values.join(", "))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// A TSV/CSV field, quoted (with `""` for quotes) when it holds the separator or a line break, and for
/// CSV also a quote. TSV leaves other quotes alone, like spreadsheets do, so JSON pastes as written.
fn delimited_field(s: &str, separator: char) -> String {
    let needs_quotes = s.contains([separator, '\n', '\r']) || (separator == ',' && s.contains('"'));
    if needs_quotes {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Column names made unique for JSON keys: a join's second `id` becomes `id_2`.
fn unique_names(columns: &[ColumnInfo]) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(columns.len());
    for column in columns {
        let mut name = column.name.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{}_{n}", column.name);
            n += 1;
        }
        names.push(name);
    }
    names
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A cell as JSON: numbers stay numbers (decimals keep every digit), `json`/`jsonb` columns are
/// embedded as JSON rather than as a string.
fn json_value(column: &ColumnInfo, value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(d) if is_json_number(d) => d.clone(),
        Value::Text(t) if column.type_name.to_ascii_lowercase().contains("json") && is_json(t) => t.trim().to_string(),
        v => json_string(&v.display()),
    }
}

fn is_json(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

fn is_json_number(text: &str) -> bool {
    matches!(serde_json::from_str::<serde_json::Value>(text), Ok(serde_json::Value::Number(_)))
}

/// A value as a literal for an `INSERT`. Binary previews (`0x…`) become the dialect's blob literal,
/// or NULL when the preview was cut short and the full value isn't known.
fn sql_literal(d: Dialect, column: &ColumnInfo, value: &Value) -> String {
    match value {
        Value::Text(t) if is_binary(column) => match t.strip_prefix("0x") {
            Some(hex) if !hex.ends_with('…') && hex.chars().all(|c| c.is_ascii_hexdigit()) => match d.0 {
                DatabaseKind::Postgres => format!("'\\x{hex}'"),
                DatabaseKind::SqlServer => format!("0x{hex}"),
                DatabaseKind::Mysql | DatabaseKind::Sqlite | DatabaseKind::Libsql => format!("X'{hex}'"),
            },
            _ => "NULL /* binary value too large to copy */".into(),
        },
        v => d.literal(v),
    }
}

/// `text` re-indented with two spaces when it's a JSON object or array, else `None`. Keys keep their
/// order and numbers their digits (the text is re-spaced, not parsed into values and printed back).
pub fn pretty_json(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) || !is_json(trimmed) {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len() * 2);
    let mut depth = 0usize;
    let mut chars = trimmed.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() {
                            out.push(e);
                        }
                    } else if d == '"' {
                        break;
                    }
                }
            }
            '{' | '[' => {
                // Empty containers stay `{}` / `[]`.
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                let close = if c == '{' { '}' } else { ']' };
                if chars.peek() == Some(&close) {
                    chars.next();
                    out.push(c);
                    out.push(close);
                } else {
                    out.push(c);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), type_name: ty.into(), is_primary_key: false, is_nullable: true }
    }

    fn sample() -> (Vec<ColumnInfo>, Vec<Vec<Value>>) {
        let columns = vec![col("id", "int8"), col("name", "text"), col("total", "numeric"), col("meta", "jsonb")];
        let rows = vec![
            vec![Value::Int(1), Value::Text("Ada, \"the\" first".into()), Value::Decimal("10.50".into()), Value::Text(r#"{"b":1,"a":[2]}"#.into())],
            vec![Value::Int(2), Value::Text("tab\there\nline".into()), Value::Null, Value::Null],
        ];
        (columns, rows)
    }

    const PG: Target = Target { kind: DatabaseKind::Postgres, schema: Some("public"), table: Some("users") };

    #[test]
    fn formats_tsv_and_csv() {
        let (columns, rows) = sample();
        assert_eq!(
            format_rows(CopyFormat::Tsv, PG, &columns, &rows, false),
            "1\tAda, \"the\" first\t10.50\t{\"b\":1,\"a\":[2]}\n2\t\"tab\there\nline\"\t\t"
        );
        let csv = format_rows(CopyFormat::Csv, PG, &columns, &rows, true);
        assert_eq!(
            csv,
            "id,name,total,meta\r\n1,\"Ada, \"\"the\"\" first\",10.50,\"{\"\"b\"\":1,\"\"a\"\":[2]}\"\r\n2,\"tab\there\nline\",,"
        );
    }

    #[test]
    fn formats_json_in_column_order() {
        let (columns, rows) = sample();
        let json = format_rows(CopyFormat::Json, PG, &columns, &rows[..1], false);
        assert_eq!(
            json,
            "[\n  {\n    \"id\": 1,\n    \"name\": \"Ada, \\\"the\\\" first\",\n    \"total\": 10.50,\n    \"meta\": {\"b\":1,\"a\":[2]}\n  }\n]"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&format_rows(CopyFormat::Json, PG, &columns, &rows, false)).is_ok());
        assert_eq!(format_rows(CopyFormat::Json, PG, &columns, &[], false), "[]");
        // Duplicate names (joins) get distinct keys.
        let dup = format_rows(CopyFormat::Json, PG, &[col("id", ""), col("id", "")], &[vec![Value::Int(1), Value::Int(2)]], false);
        assert!(dup.contains("\"id_2\": 2"));
    }

    #[test]
    fn formats_markdown() {
        let (columns, rows) = sample();
        let md = format_rows(CopyFormat::Markdown, PG, &columns[..2], &[vec![Value::Int(1), Value::Text("a|b\nc".into())]], false);
        assert_eq!(md, "| id | name |\n| --- | --- |\n| 1 | a\\|b<br>c |");
        let _ = rows;
    }

    #[test]
    fn formats_inserts_per_dialect() {
        let (columns, rows) = sample();
        assert_eq!(
            format_rows(CopyFormat::Insert, PG, &columns, &rows[1..], false),
            "insert into \"public\".\"users\" (\"id\", \"name\", \"total\", \"meta\") values (2, 'tab\there\nline', null, null);"
        );
        let my = Target { kind: DatabaseKind::Mysql, schema: Some("shop"), table: Some("t") };
        let cols = [col("flag", "tinyint(1)"), col("data", "blob"), col("big", "longblob")];
        let row = vec![Value::Bool(true), Value::Text("0x00ff".into()), Value::Text("0xab…".into())];
        assert_eq!(
            format_rows(CopyFormat::Insert, my, &cols, &[row], false),
            "insert into `shop`.`t` (`flag`, `data`, `big`) values (1, X'00ff', NULL /* binary value too large to copy */);"
        );
        let script = Target { kind: DatabaseKind::Sqlite, schema: None, table: None };
        assert!(format_rows(CopyFormat::Insert, script, &cols[..1], &[vec![Value::Int(1)]], false).starts_with("insert into \"table_name\""));
    }

    #[test]
    fn pretty_prints_json_keeping_order_and_digits() {
        assert_eq!(
            pretty_json(r#" {"z": 1.000000000000000000001, "a": [], "s": "x,{y}:\"", "o": {"k": [1, 2]}} "#).unwrap(),
            "{\n  \"z\": 1.000000000000000000001,\n  \"a\": [],\n  \"s\": \"x,{y}:\\\"\",\n  \"o\": {\n    \"k\": [\n      1,\n      2\n    ]\n  }\n}"
        );
        assert_eq!(pretty_json("[]").unwrap(), "[]");
        assert_eq!(pretty_json("42"), None);
        assert_eq!(pretty_json("{oops"), None);
        assert_eq!(pretty_json("plain text"), None);
    }
}
