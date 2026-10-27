//! Splits a SQL script into statements, line by line, without loading the file.
//!
//! Knows each dialect's quoting and comments (`'…'`, `E'…'`, `"…"`, `` `…` ``, `[…]`, `$tag$…$tag$`,
//! `--`, `#`, nested `/* */`), MySQL's `DELIMITER`, psql's `COPY … FROM stdin` data blocks
//! (ended by `\.`) and meta-commands (`\connect`, `\restrict`…), and statements whose bodies
//! contain `;` (`CREATE TRIGGER … BEGIN … END`, SQL-standard function bodies `BEGIN ATOMIC … END`).
//! SQL Server scripts are split into batches on `GO` lines instead, like sqlcmd (`;` doesn't split).

use crate::model::DatabaseKind;

/// Data of a `COPY … FROM stdin` block is handed on in pieces of about this size.
const COPY_PIECE: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A statement without its delimiter. `line` is where it starts (1-based).
    Statement { sql: String, line: usize },
    /// `COPY … FROM stdin`: its rows follow as `CopyData`, then `CopyEnd`.
    CopyStart { sql: String, line: usize },
    CopyData(Vec<u8>),
    CopyEnd,
    /// A psql meta-command (`\connect db`), which isn't SQL.
    Meta { command: String, line: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Normal,
    /// `backslash`: `\` escapes the next character (MySQL strings, Postgres `E'…'`).
    Quoted { close: char, backslash: bool },
    Dollar(String),
    LineComment,
    BlockComment(u32),
}

pub struct Splitter {
    kind: DatabaseKind,
    delimiter: String,
    buffer: String,
    state: State,
    /// The buffer has more than whitespace and comments.
    has_code: bool,
    start_line: usize,
    line: usize,
    /// First words of the statement (upper case), to recognise `COPY` and routine bodies.
    head: Vec<String>,
    word: String,
    /// Open `BEGIN`/`CASE` blocks inside a routine or trigger body.
    depth: u32,
    previous: char,
    in_copy: bool,
    copy: Vec<u8>,
    /// SQL Server: the line after the last `GO`, where the batch's text starts.
    batch_start: usize,
}

impl Splitter {
    pub fn new(kind: DatabaseKind) -> Self {
        // Turso / libSQL and D1 speak SQLite's SQL.
        let kind = if matches!(kind, DatabaseKind::Libsql | DatabaseKind::D1) { DatabaseKind::Sqlite } else { kind };
        Self {
            kind,
            // SQL Server batches end at `GO` lines, never at `;`.
            delimiter: if kind == DatabaseKind::SqlServer { String::new() } else { ";".into() },
            buffer: String::new(),
            state: State::Normal,
            has_code: false,
            start_line: 1,
            line: 0,
            head: Vec::new(),
            word: String::new(),
            depth: 0,
            previous: ' ',
            in_copy: false,
            copy: Vec::new(),
            batch_start: 1,
        }
    }

    fn sqlserver(&self) -> bool {
        self.kind == DatabaseKind::SqlServer
    }

    fn mysql(&self) -> bool {
        self.kind == DatabaseKind::Mysql
    }

    fn postgres(&self) -> bool {
        self.kind == DatabaseKind::Postgres
    }

    /// Inside a `COPY … FROM stdin` block: the next lines are data, not SQL (and may not be UTF-8).
    pub fn in_copy(&self) -> bool {
        self.in_copy
    }

    /// Lines read so far.
    pub fn line(&self) -> usize {
        self.line
    }

    /// One data line (with its newline) of a `COPY` block.
    pub fn push_copy_line(&mut self, line: &[u8], out: &mut Vec<Item>) {
        self.line += 1;
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content == b"\\." {
            if !self.copy.is_empty() {
                out.push(Item::CopyData(std::mem::take(&mut self.copy)));
            }
            out.push(Item::CopyEnd);
            self.in_copy = false;
            return;
        }
        self.copy.extend_from_slice(line);
        if self.copy.len() >= COPY_PIECE {
            out.push(Item::CopyData(std::mem::take(&mut self.copy)));
        }
    }

    /// One line of SQL, with its newline (the last line may lack it).
    pub fn push_line(&mut self, line: &str, out: &mut Vec<Item>) {
        if self.in_copy {
            return self.push_copy_line(line.as_bytes(), out);
        }
        self.line += 1;
        if self.sqlserver() && self.state == State::Normal {
            if let Some(repeat) = crate::sqlserver::script::go_line(line) {
                self.end_word();
                let start = self.batch_start;
                for _ in 0..repeat {
                    self.start_line = start;
                    let (buffer, has_code) = (self.buffer.clone(), self.has_code);
                    self.end_statement(out);
                    self.buffer = buffer;
                    self.has_code = has_code;
                }
                self.buffer.clear();
                self.has_code = false;
                self.batch_start = self.line + 1;
                return;
            }
        }
        if self.state == State::Normal && !self.has_code && !self.sqlserver() {
            let trimmed = line.trim();
            if !self.mysql() && trimmed.starts_with('\\') {
                out.push(Item::Meta { command: trimmed.to_string(), line: self.line });
                return;
            }
            if self.mysql() && trimmed.get(..10).is_some_and(|w| w.eq_ignore_ascii_case("delimiter ")) {
                self.delimiter = trimmed[10..].trim().to_string();
                return;
            }
        }

        let mut i = 0;
        while i < line.len() {
            let rest = &line[i..];
            let c = rest.chars().next().expect("non-empty");
            let mut step = c.len_utf8();
            match self.state.clone() {
                State::Normal => {
                    if !(c.is_alphanumeric() || c == '_') {
                        // A word just ended (`END` before `;`): count it before looking at the delimiter.
                        self.end_word();
                    }
                    if !self.delimiter.is_empty() && rest.starts_with(self.delimiter.as_str()) && !(self.delimiter == ";" && self.depth > 0) {
                        self.end_word();
                        self.end_statement(out);
                        i += self.delimiter.len();
                        self.previous = ' ';
                        continue;
                    }
                    if c.is_alphanumeric() || c == '_' {
                        self.word.push(c);
                        self.code();
                    } else {
                        self.end_word();
                        match c {
                            '\'' => {
                                let escape = self.mysql() || (self.postgres() && self.previous_is_e_prefix());
                                self.state = State::Quoted { close: '\'', backslash: escape };
                                self.code();
                            }
                            '"' => {
                                self.state = State::Quoted { close: '"', backslash: self.mysql() };
                                self.code();
                            }
                            '`' if self.mysql() => {
                                self.state = State::Quoted { close: '`', backslash: false };
                                self.code();
                            }
                            '[' if matches!(self.kind, DatabaseKind::Sqlite | DatabaseKind::SqlServer) => {
                                self.state = State::Quoted { close: ']', backslash: false };
                                self.code();
                            }
                            '$' if self.postgres() && !is_ident(self.previous) => {
                                if let Some(tag) = dollar_tag(rest) {
                                    step = tag.len();
                                    self.state = State::Dollar(tag.to_string());
                                }
                                self.code();
                            }
                            '-' if rest.starts_with("--")
                                && (!self.mysql() || rest[2..].chars().next().is_none_or(char::is_whitespace)) =>
                            {
                                self.state = State::LineComment;
                            }
                            '#' if self.mysql() => self.state = State::LineComment,
                            '/' if rest.starts_with("/*") => {
                                // MySQL's `/*! … */` and `/*+ … */` are executed, not ignored.
                                if self.mysql() && (rest.starts_with("/*!") || rest.starts_with("/*+")) {
                                    self.code();
                                }
                                self.state = State::BlockComment(1);
                                step = 2;
                            }
                            c if c.is_whitespace() => {}
                            _ => self.code(),
                        }
                    }
                }
                State::Quoted { close, backslash } => {
                    if backslash && c == '\\' {
                        // Keep the escaped character as is.
                        if let Some(next) = rest[1..].chars().next() {
                            step += next.len_utf8();
                        }
                    } else if c == close {
                        if rest[c.len_utf8()..].starts_with(close) {
                            step += close.len_utf8();
                        } else {
                            self.state = State::Normal;
                        }
                    }
                }
                State::Dollar(tag) => {
                    if rest.starts_with(tag.as_str()) {
                        step = tag.len();
                        self.state = State::Normal;
                    }
                }
                State::LineComment => {
                    if c == '\n' {
                        self.state = State::Normal;
                    }
                }
                State::BlockComment(depth) => {
                    if rest.starts_with("*/") {
                        step = 2;
                        self.state = if depth == 1 { State::Normal } else { State::BlockComment(depth - 1) };
                    } else if rest.starts_with("/*") && (self.postgres() || self.sqlserver()) {
                        step = 2;
                        self.state = State::BlockComment(depth + 1);
                    }
                }
            }
            self.buffer.push_str(&line[i..i + step]);
            self.previous = line[i..i + step].chars().last().unwrap_or(' ');
            i += step;
        }
        if self.state == State::Normal {
            self.end_word();
        }
    }

    /// The end of the script: a last statement without its delimiter still runs.
    pub fn finish(&mut self, out: &mut Vec<Item>) {
        if self.in_copy {
            if !self.copy.is_empty() {
                out.push(Item::CopyData(std::mem::take(&mut self.copy)));
            }
            out.push(Item::CopyEnd);
            self.in_copy = false;
        }
        self.end_word();
        if self.sqlserver() {
            self.start_line = self.batch_start;
        }
        self.end_statement(out);
    }

    fn code(&mut self) {
        if !self.has_code {
            self.has_code = true;
            self.start_line = self.line;
        }
    }

    fn previous_is_e_prefix(&self) -> bool {
        // `E'…'`: the word just before the quote is exactly `E`.
        matches!(self.previous, 'e' | 'E') && {
            let before = self.buffer.trim_end_matches(['e', 'E']);
            self.buffer.len() - before.len() == 1 && !before.chars().last().is_some_and(is_ident)
        }
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word).to_uppercase();
        if self.head.len() < 6 {
            self.head.push(word.clone());
        }
        if !self.mysql() && !self.sqlserver() && self.has_body() {
            match word.as_str() {
                "BEGIN" | "CASE" => self.depth += 1,
                "END" => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
    }

    /// `CREATE [OR REPLACE] [TEMP] [CONSTRAINT] {FUNCTION | PROCEDURE | TRIGGER}`: a body that may hold `;`.
    fn has_body(&self) -> bool {
        self.head.first().is_some_and(|w| w == "CREATE")
            && self.head.iter().skip(1).take(4).any(|w| matches!(w.as_str(), "FUNCTION" | "PROCEDURE" | "TRIGGER"))
    }

    fn end_statement(&mut self, out: &mut Vec<Item>) {
        // A SQL Server batch is sent exactly as written (module definitions keep their text, and
        // the server's line numbers count from the batch's first line).
        let sql = if self.sqlserver() { self.buffer.clone() } else { self.buffer.trim().to_string() };
        let has_code = self.has_code;
        let is_copy_from_stdin = self.postgres() && self.head.first().is_some_and(|w| w == "COPY") && copy_from_stdin(&sql);
        self.buffer.clear();
        self.head.clear();
        self.word.clear();
        self.has_code = false;
        self.depth = 0;
        self.state = State::Normal;
        if !has_code {
            return;
        }
        let line = self.start_line;
        if is_copy_from_stdin {
            self.in_copy = true;
            out.push(Item::CopyStart { sql, line });
        } else {
            out.push(Item::Statement { sql, line });
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `$$` or `$tag$` at the start of `text`.
fn dollar_tag(text: &str) -> Option<&str> {
    let body = &text[1..];
    let end = body.find('$')?;
    let tag = &body[..end];
    let valid = tag.chars().all(|c| c.is_alphanumeric() || c == '_') && !tag.starts_with(|c: char| c.is_ascii_digit());
    valid.then(|| &text[..end + 2])
}

/// `COPY … FROM stdin` (data follows in the script), not `COPY … FROM 'file'` or `TO stdout`.
fn copy_from_stdin(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    let words: Vec<&str> = upper.split(|c: char| c.is_whitespace() || c == '(' || c == ')').filter(|w| !w.is_empty()).collect();
    words.windows(2).any(|w| w[0] == "FROM" && w[1] == "STDIN")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(kind: DatabaseKind, script: &str) -> Vec<Item> {
        let mut splitter = Splitter::new(kind);
        let mut out = Vec::new();
        for line in script.split_inclusive('\n') {
            splitter.push_line(line, &mut out);
        }
        splitter.finish(&mut out);
        out
    }

    fn statements(kind: DatabaseKind, script: &str) -> Vec<String> {
        split(kind, script)
            .into_iter()
            .map(|i| match i {
                Item::Statement { sql, .. } => sql,
                other => format!("{other:?}"),
            })
            .collect()
    }

    const PG: DatabaseKind = DatabaseKind::Postgres;
    const MY: DatabaseKind = DatabaseKind::Mysql;
    const LITE: DatabaseKind = DatabaseKind::Sqlite;

    #[test]
    fn splits_simple_statements() {
        assert_eq!(statements(PG, "select 1; select 2;\nselect 3"), ["select 1", "select 2", "select 3"]);
        assert_eq!(statements(PG, "-- only a comment;\n\n;  ;"), Vec::<String>::new());
        let items = split(PG, "\n\n  select\n 1;\n-- c\nselect 2;");
        assert_eq!(items[0], Item::Statement { sql: "select\n 1".into(), line: 3 });
        assert_eq!(items[1], Item::Statement { sql: "-- c\nselect 2".into(), line: 6 });
    }

    #[test]
    fn skips_semicolons_in_quotes_and_comments() {
        assert_eq!(statements(PG, "select 'a;b', \"c;d\" /* ; /* nested ; */ ; */ -- ;\n;"), ["select 'a;b', \"c;d\" /* ; /* nested ; */ ; */ -- ;"]);
        assert_eq!(statements(PG, "select 'it''s;'; select 2"), ["select 'it''s;'", "select 2"]);
        assert_eq!(statements(PG, r"select E'\';', 'a\'; select 2"), [r"select E'\';', 'a\'", "select 2"]);
        assert_eq!(statements(MY, r"select 'a\';b', `x;y` # ;
;"), [r"select 'a\';b', `x;y` # ;"]);
        assert_eq!(statements(MY, "select 1--1;\nselect 2"), ["select 1--1", "select 2"]);
        assert_eq!(statements(LITE, "select [a;b] from t; select 2"), ["select [a;b] from t", "select 2"]);
    }

    #[test]
    fn handles_dollar_quotes() {
        let f = "create function f() returns int as $body$\nbegin\n  return 1; -- $$ ;\nend;\n$body$ language plpgsql;\nselect $1, $$a;b$$;";
        assert_eq!(
            statements(PG, f),
            [
                "create function f() returns int as $body$\nbegin\n  return 1; -- $$ ;\nend;\n$body$ language plpgsql",
                "select $1, $$a;b$$"
            ]
        );
    }

    #[test]
    fn keeps_bodies_whole() {
        let trigger = "CREATE TRIGGER t AFTER INSERT ON a BEGIN\n  UPDATE b SET n = CASE WHEN 1 THEN 2 END;\n  DELETE FROM c;\nEND;\nselect 1;";
        assert_eq!(statements(LITE, trigger)[0], trigger[..trigger.len() - 11]);
        assert_eq!(statements(LITE, trigger)[1], "select 1");
        let atomic = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC\n SELECT 1;\n SELECT 2;\nEND;\nBEGIN;\nCOMMIT;";
        assert_eq!(statements(PG, atomic).len(), 3);
        assert_eq!(statements(PG, "BEGIN; select 1; END;"), ["BEGIN", "select 1", "END"]);
    }

    #[test]
    fn mysql_delimiters() {
        let script = "DELIMITER ;;\nCREATE PROCEDURE p() BEGIN select 1; select 2; END ;;\nDELIMITER ;\nselect 3;\n/*!40101 SET NAMES utf8mb4 */;";
        assert_eq!(
            statements(MY, script),
            ["CREATE PROCEDURE p() BEGIN select 1; select 2; END", "select 3", "/*!40101 SET NAMES utf8mb4 */"]
        );
    }

    #[test]
    fn sqlserver_go_batches() {
        const MS: DatabaseKind = DatabaseKind::SqlServer;
        let script = "-- header\nSET NOCOUNT ON;\nselect 1;\nGO\n\nCREATE PROCEDURE p AS\nBEGIN\n  select 'GO'; select [a;b]\nEND\ngo 2\n/* GO\n*/ select 2\n";
        assert_eq!(
            split(MS, script),
            [
                Item::Statement { sql: "-- header\nSET NOCOUNT ON;\nselect 1;\n".into(), line: 1 },
                Item::Statement { sql: "\nCREATE PROCEDURE p AS\nBEGIN\n  select 'GO'; select [a;b]\nEND\n".into(), line: 5 },
                Item::Statement { sql: "\nCREATE PROCEDURE p AS\nBEGIN\n  select 'GO'; select [a;b]\nEND\n".into(), line: 5 },
                Item::Statement { sql: "/* GO\n*/ select 2\n".into(), line: 11 },
            ]
        );
    }

    #[test]
    fn copy_blocks_and_meta_commands() {
        let script = "\\restrict abc\nSET x = 1;\nCOPY public.t (a, b) FROM stdin;\n1\ta;b\n2\t\\N\n\\.\nselect 1;\n\\unrestrict abc\n";
        assert_eq!(
            split(PG, script),
            [
                Item::Meta { command: "\\restrict abc".into(), line: 1 },
                Item::Statement { sql: "SET x = 1".into(), line: 2 },
                Item::CopyStart { sql: "COPY public.t (a, b) FROM stdin".into(), line: 3 },
                Item::CopyData(b"1\ta;b\n2\t\\N\n".to_vec()),
                Item::CopyEnd,
                Item::Statement { sql: "select 1".into(), line: 7 },
                Item::Meta { command: "\\unrestrict abc".into(), line: 8 },
            ]
        );
        // An empty COPY block; COPY to a file isn't followed by data.
        assert_eq!(
            split(PG, "COPY t FROM STDIN;\r\n\\.\r\nCOPY t TO '/tmp/x';\n"),
            [
                Item::CopyStart { sql: "COPY t FROM STDIN".into(), line: 1 },
                Item::CopyEnd,
                Item::Statement { sql: "COPY t TO '/tmp/x'".into(), line: 3 },
            ]
        );
    }
}
