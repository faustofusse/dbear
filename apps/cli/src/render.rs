//! Output: an aligned table for people (unicode-width aware, NULL set apart, wide cells cut on a
//! terminal only), and `dbcore::export` for TSV, CSV, JSON, Markdown and `INSERT`s.

use std::io::IsTerminal;

use dbcore::export::{self, CopyFormat};
use dbcore::{ColumnInfo, DatabaseKind, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Aligned columns (the default on a terminal).
    Table,
    /// Tab-separated with a header line (the default when piped). NULL is an empty field.
    Tsv,
    Csv,
    /// An array of objects, keys in column order.
    Json,
    Markdown,
    /// `INSERT` statements.
    Insert,
}

/// How the table looks.
#[derive(Debug, Clone, Default)]
pub struct Style {
    pub color: bool,
    /// Cut cells wider than this (with `…`).
    pub max_cell: Option<usize>,
    /// Fit the table in this many columns, cutting the widest columns (a terminal's width).
    pub width: Option<usize>,
}

/// Cells wider than this are cut on a terminal unless `--max-width` says otherwise.
pub const DEFAULT_MAX_CELL: usize = 64;
/// Columns aren't narrowed below this to fit the terminal.
const MIN_COLUMN: usize = 8;

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Colors on a terminal, unless `NO_COLOR` is set or `TERM=dumb`.
pub fn colors() -> bool {
    stdout_is_tty() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

/// The terminal's width (stdout), when it is one.
pub fn terminal_width() -> Option<usize> {
    if !stdout_is_tty() {
        return None;
    }
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ fills a winsize on a terminal and fails otherwise.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
            return Some(size.ws_col as usize);
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|&c| c > 0)
}

/// The style for stdout: on a terminal colors, a cell cap (`max_width`: 0 for none) and its width;
/// piped, nothing is cut unless `max_width` asks.
pub fn style(max_width: Option<usize>) -> Style {
    let tty = stdout_is_tty();
    let cap = match max_width {
        Some(0) => None,
        Some(n) => Some(n),
        None => tty.then_some(DEFAULT_MAX_CELL),
    };
    Style { color: colors(), max_cell: cap, width: if tty && max_width != Some(0) { terminal_width() } else { None } }
}

/// The format to use: the one asked for, else a table on a terminal and TSV when piped.
pub fn pick(format: Option<Format>) -> Format {
    format.unwrap_or(if stdout_is_tty() { Format::Table } else { Format::Tsv })
}

/// A text column for listings.
pub fn column(name: &str) -> ColumnInfo {
    ColumnInfo { name: name.into(), type_name: "text".into(), is_primary_key: false, is_nullable: true }
}

pub fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

pub fn optional(s: Option<impl Into<String>>) -> Value {
    s.map_or(Value::Null, |s| Value::Text(s.into()))
}

/// `rows` in `format`, ending in a newline (nothing at all for no columns).
pub fn grid(format: Format, kind: DatabaseKind, columns: &[ColumnInfo], rows: &[Vec<Value>], style: &Style) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let copy = match format {
        Format::Table => return table(columns, rows, style),
        Format::Tsv => CopyFormat::Tsv,
        Format::Csv => CopyFormat::Csv,
        Format::Json => CopyFormat::Json,
        Format::Markdown => CopyFormat::Markdown,
        Format::Insert => CopyFormat::Insert,
    };
    let mut out = export::format_rows(copy, export::Target { kind, schema: None, table: None }, columns, rows, true);
    if !out.is_empty() {
        out.push_str(if copy == CopyFormat::Csv { "\r\n" } else { "\n" });
    }
    out
}

struct Cell {
    text: String,
    null: bool,
}

fn is_number(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_) | Value::Decimal(_))
}

/// One line per cell: control characters (line breaks, tabs) are shown escaped.
fn visible(s: &str) -> String {
    if !s.chars().any(char::is_control) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// `s` cut to `max` display columns, ending in `…` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let room = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// ` id │ name ` columns with a rule under the header. Numbers are right-aligned; NULL is dim
/// `NULL` in color, else `<null>`, so it never looks like the text "NULL" or an empty string.
pub fn table(columns: &[ColumnInfo], rows: &[Vec<Value>], style: &Style) -> String {
    let null_text = if style.color { "NULL" } else { "<null>" };
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|i| match row.get(i).unwrap_or(&Value::Null) {
                    Value::Null => Cell { text: null_text.into(), null: true },
                    v => Cell { text: visible(&v.display()), null: false },
                })
                .collect()
        })
        .collect();
    let right: Vec<bool> = (0..columns.len())
        .map(|i| {
            let mut values = rows.iter().filter_map(|r| r.get(i)).filter(|v| !v.is_null()).peekable();
            values.peek().is_some() && values.all(is_number)
        })
        .collect();
    let headers: Vec<String> = columns.iter().map(|c| visible(&c.name)).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| width(h)).collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(width(&cell.text));
        }
    }
    if let Some(cap) = style.max_cell {
        for w in &mut widths {
            *w = (*w).min(cap.max(1));
        }
    }
    if let Some(total) = style.width {
        // " a │ b " is the widths plus 3 per column, minus the last one's trailing space.
        let used = |widths: &[usize]| widths.iter().sum::<usize>() + 3 * widths.len() - 1;
        while used(&widths) > total {
            let Some((i, _)) = widths.iter().enumerate().filter(|(_, w)| **w > MIN_COLUMN).max_by_key(|(_, w)| **w) else { break };
            widths[i] -= 1;
        }
    }
    let paint = |s: &str, code: &str| if style.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() };
    let last = columns.len() - 1;
    let line = |texts: Vec<(String, bool, bool)>| {
        let mut out = String::new();
        for (i, (text, null, header)) in texts.into_iter().enumerate() {
            out.push_str(if i == 0 { " " } else { " │ " });
            let text = truncate(&text, widths[i]);
            let pad = " ".repeat(widths[i] - width(&text));
            let painted = if header {
                paint(&text, "1")
            } else if null {
                paint(&text, "2")
            } else {
                text
            };
            if right[i] {
                out.push_str(&pad);
                out.push_str(&painted);
            } else {
                out.push_str(&painted);
                if i != last {
                    out.push_str(&pad);
                }
            }
        }
        out.truncate(out.trim_end().len());
        out.push('\n');
        out
    };
    let mut out = line(headers.into_iter().map(|h| (h, false, true)).collect());
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
    out.push_str(&rule.join("┼"));
    out.push('\n');
    for row in cells {
        out.push_str(&line(row.into_iter().map(|c| (c.text, c.null, false)).collect()));
    }
    out
}

/// `1 row`, `3 rows`.
pub fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `1.2 MB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Milliseconds since the epoch as `2025-06-01 12:03:04` (UTC).
pub fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    format!("{} {:02}:{:02}:{:02}", dbservice::civil_date(days), rest / 3600, rest % 3600 / 60, rest % 60)
}
