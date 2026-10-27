//! Native SQL completion: keywords, schemas, tables/views and columns, from the schema the app
//! already loads (`list_schemas` + `list_columns`). No external process, no second connection.
//!
//! This understands just enough SQL to place the cursor in context (which clause, and after
//! which relation or alias), by scanning tokens rather than requiring a full parse: scripts are
//! usually mid-edit and wouldn't parse anyway. Offsets are UTF-8 byte offsets, like
//! [`crate::highlight`]; the FFI layer converts them to UTF-16.

use crate::dialect::Dialect;
use crate::model::{ColumnInfo, DatabaseKind, Schema, TableColumns, TableKind};

/// A schema's tables and views, with their columns, ready for completion.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    schemas: Vec<CatalogSchema>,
}

#[derive(Debug, Clone)]
struct CatalogSchema {
    name: String,
    tables: Vec<CatalogTable>,
}

#[derive(Debug, Clone)]
struct CatalogTable {
    name: String,
    kind: TableKind,
    columns: Vec<ColumnInfo>,
}

impl Catalog {
    /// Builds a catalog from one `list_schemas` and one `list_columns` call.
    pub fn new(schemas: Vec<Schema>, columns: Vec<TableColumns>) -> Self {
        let mut out: Vec<CatalogSchema> = schemas
            .into_iter()
            .map(|s| CatalogSchema {
                name: s.name,
                tables: s.tables.into_iter().map(|t| CatalogTable { name: t.name, kind: t.kind, columns: Vec::new() }).collect(),
            })
            .collect();
        for tc in columns {
            if let Some(schema) = out.iter_mut().find(|s| s.name == tc.schema) {
                if let Some(table) = schema.tables.iter_mut().find(|t| t.name == tc.table) {
                    table.columns = tc.columns;
                }
            }
        }
        Self { schemas: out }
    }

    fn tables<'a>(&'a self, schema: Option<&'a str>) -> impl Iterator<Item = (&'a str, &'a CatalogTable)> {
        self.schemas.iter().filter(move |s| schema.is_none_or(|n| eq_ident(&s.name, n))).flat_map(|s| {
            s.tables.iter().map(move |t| (s.name.as_str(), t))
        })
    }

    /// Case-insensitive, unqualified table/alias lookup. Ambiguous names (the same table name in
    /// several schemas) prefer `public`, like Postgres' default `search_path`.
    fn find_table(&self, name: &str) -> Option<&CatalogTable> {
        let mut candidates: Vec<(&str, &CatalogTable)> = self.tables(None).filter(|(_, t)| eq_ident(&t.name, name)).collect();
        candidates.sort_by_key(|(schema, _)| !eq_ident(schema, "public"));
        candidates.into_iter().map(|(_, t)| t).next()
    }

    fn find_schema(&self, name: &str) -> Option<&CatalogSchema> {
        self.schemas.iter().find(|s| eq_ident(&s.name, name))
    }
}

fn eq_ident(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completions {
    /// Byte range of `sql` to replace with `insert_text` (the word being typed).
    pub replace_start: usize,
    pub replace_end: usize,
    pub items: Vec<CompletionItem>,
}

const MAX_ITEMS: usize = 200;

const KEYWORDS: &[&str] = &[
    "select", "from", "where", "join", "inner join", "left join", "right join", "full join", "cross join", "on",
    "and", "or", "not", "in", "exists", "between", "like", "ilike", "is null", "is not null", "group by", "order by",
    "having", "limit", "offset", "as", "distinct", "union", "union all", "insert into", "values", "update", "set",
    "delete from", "create table", "create index", "create view", "alter table", "drop table", "with", "case",
    "when", "then", "else", "end", "asc", "desc", "returning", "using",
];

/// Main entry point: completions for `sql` with the caret at byte `offset`.
pub fn complete(sql: &str, offset: usize, catalog: &Catalog, dialect: Dialect) -> Completions {
    let offset = offset.min(sql.len());
    let tokens = tokenize(sql);
    let stmt = statement_bounds(&tokens, offset);
    let stmt_tokens: Vec<&Token> = tokens.iter().filter(|t| t.start >= stmt.0 && t.end <= stmt.1).collect();

    let (replace_start, replace_end, prefix) = current_word(sql, &tokens, offset);
    let qualifier = qualifier_before(sql, &tokens, replace_start);
    let boundary = qualifier.as_ref().map(|q| q.dot_start).unwrap_or(replace_start);
    let keyword_ctx = preceding_keyword(sql, &stmt_tokens, boundary);

    let relations = collect_relations(sql, &stmt_tokens);

    let mut items = if let Some(q) = &qualifier {
        qualified_candidates(sql, &q.name, &relations, catalog, 0)
    } else {
        match keyword_ctx {
            KeywordContext::Relation => relation_candidates(catalog),
            KeywordContext::Column => column_candidates(sql, &relations, catalog, dialect, 0),
            KeywordContext::Start => statement_start_candidates(),
        }
    };

    filter_and_rank(&mut items, &prefix);
    items.truncate(MAX_ITEMS);
    Completions { replace_start, replace_end, items }
}

/// Completions inside a table's `WHERE` filter (the search field of a table tab): completed as if
/// typed after `SELECT * FROM schema.table WHERE`, so the table's columns and condition keywords
/// come first. `offset` and the returned range are relative to `filter`.
pub fn complete_filter(filter: &str, offset: usize, catalog: &Catalog, dialect: Dialect, schema: &str, table: &str) -> Completions {
    let prefix = format!("select * from {} where ", dialect.quote_relation(schema, table));
    let sql = format!("{prefix}{filter}");
    let mut result = complete(&sql, prefix.len() + offset.min(filter.len()), catalog, dialect);
    result.replace_start = result.replace_start.saturating_sub(prefix.len());
    result.replace_end = result.replace_end.saturating_sub(prefix.len());
    result
}

// MARK: Tokenizing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Word,
    QuotedWord,
    Dot,
    Comma,
    OpenParen,
    CloseParen,
    Semicolon,
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokKind,
    start: usize,
    end: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenizes `sql`, skipping whitespace, comments and string literals (they never matter for
/// completion context). Quoted identifiers (`"..."`, `` `...` ``) become `QuotedWord`.
fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let char_at = |i: usize| sql[i..].chars().next();

    while i < len {
        let c = char_at(i).unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        // Line comment.
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < len && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(len);
            continue;
        }
        // String literal: '...'  ('' is an escaped quote).
        if c == '\'' {
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += char_at(i).map(|c| c.len_utf8()).unwrap_or(1);
            }
            continue;
        }
        // Quoted identifier: "..." or `...`.
        if c == '"' || c == '`' {
            let quote = c as u8;
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += char_at(i).map(|c| c.len_utf8()).unwrap_or(1);
            }
            tokens.push(Token { kind: TokKind::QuotedWord, start, end: i });
            continue;
        }
        if is_ident_char(c) {
            let start = i;
            while i < len {
                let Some(c) = char_at(i) else { break };
                if !is_ident_char(c) {
                    break;
                }
                i += c.len_utf8();
            }
            tokens.push(Token { kind: TokKind::Word, start, end: i });
            continue;
        }
        let kind = match c {
            '.' => TokKind::Dot,
            ',' => TokKind::Comma,
            '(' => TokKind::OpenParen,
            ')' => TokKind::CloseParen,
            ';' => TokKind::Semicolon,
            _ => TokKind::Other,
        };
        tokens.push(Token { kind, start: i, end: i + c.len_utf8() });
        i += c.len_utf8();
    }
    tokens
}

fn token_text<'a>(sql: &'a str, t: &Token) -> &'a str {
    &sql[t.start..t.end]
}

/// Identifier text with surrounding quotes stripped and doubled quotes collapsed.
fn dequote(sql: &str, t: &Token) -> String {
    let raw = token_text(sql, t);
    match t.kind {
        TokKind::QuotedWord => {
            let inner = &raw[1..raw.len().saturating_sub(1)];
            let quote = raw.as_bytes()[0] as char;
            inner.replace(&format!("{quote}{quote}"), &quote.to_string())
        }
        _ => raw.to_string(),
    }
}

fn is_name(t: &Token) -> bool {
    matches!(t.kind, TokKind::Word | TokKind::QuotedWord)
}

/// `(start, end)` byte range of the statement containing `offset` (between the surrounding `;`s).
fn statement_bounds(tokens: &[Token], offset: usize) -> (usize, usize) {
    let mut start = 0;
    let mut end = usize::MAX;
    for t in tokens {
        if t.kind == TokKind::Semicolon {
            if t.end <= offset {
                start = t.end;
            } else if end == usize::MAX {
                end = t.start;
            }
        }
    }
    (start, end)
}

/// The word under/just before the caret: its byte range and the (lowercased) prefix typed so far.
fn current_word(sql: &str, tokens: &[Token], offset: usize) -> (usize, usize, String) {
    for t in tokens {
        if is_name(t) && t.start <= offset && offset <= t.end {
            let text = if t.kind == TokKind::QuotedWord { token_text(sql, t) } else { &sql[t.start..t.end] };
            let prefix = if t.kind == TokKind::QuotedWord {
                text.trim_start_matches(['"', '`']).to_string()
            } else {
                sql[t.start..offset].to_string()
            };
            return (t.start, t.end, prefix.to_lowercase());
        }
    }
    (offset, offset, String::new())
}

struct Qualifier {
    name: String,
    /// Start of the `.` token, used as the left edge for finding the preceding keyword.
    dot_start: usize,
}

/// If the word at `word_start` is preceded by `ident.`, the dequoted qualifier name.
fn qualifier_before(sql: &str, tokens: &[Token], word_start: usize) -> Option<Qualifier> {
    let dot_idx = tokens.iter().position(|t| t.kind == TokKind::Dot && t.end == word_start)?;
    if dot_idx == 0 {
        return None;
    }
    let name_tok = &tokens[dot_idx - 1];
    if !is_name(name_tok) {
        return None;
    }
    Some(Qualifier { name: dequote(sql, name_tok), dot_start: tokens[dot_idx].start })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeywordContext {
    /// After FROM / JOIN / INTO / UPDATE / TABLE: expects a relation.
    Relation,
    /// After SELECT / WHERE / ON / AND / OR / GROUP BY / ORDER BY / SET / HAVING / a comma in one
    /// of those clauses: expects a column, function or keyword.
    Column,
    /// Nothing recognized before the cursor in this statement.
    Start,
}

const RELATION_KEYWORDS: &[&str] = &["from", "join", "into", "update", "table"];
const COLUMN_KEYWORDS: &[&str] =
    &["select", "where", "on", "and", "or", "group", "order", "set", "having", "by", "when", "case", "values"];

/// Scans backward from `boundary` for the nearest keyword that sets the completion context.
fn preceding_keyword(sql: &str, stmt_tokens: &[&Token], boundary: usize) -> KeywordContext {
    for t in stmt_tokens.iter().rev().filter(|t| t.end <= boundary) {
        if t.kind != TokKind::Word {
            continue;
        }
        let word = token_text(sql, t).to_lowercase();
        if RELATION_KEYWORDS.contains(&word.as_str()) {
            return KeywordContext::Relation;
        }
        if COLUMN_KEYWORDS.contains(&word.as_str()) {
            return KeywordContext::Column;
        }
    }
    KeywordContext::Start
}

/// A table or CTE reference found in the statement, with its optional alias.
struct Relation {
    schema: Option<String>,
    name: String,
    alias: Option<String>,
    /// `true` for a `WITH` CTE.
    is_cte: bool,
    /// Byte range of the CTE's parenthesized body (its `SELECT`), for resolving its columns.
    cte_body: Option<(usize, usize)>,
}

/// Collects every relation named in `FROM`/`JOIN`/`INTO`/`UPDATE` clauses and `WITH` CTEs.
/// Heuristic, not a parser: good enough for completion, tolerant of unfinished SQL.
fn collect_relations(sql: &str, stmt_tokens: &[&Token]) -> Vec<Relation> {
    let mut relations = Vec::new();
    let mut i = 0usize;
    while i < stmt_tokens.len() {
        let t = stmt_tokens[i];
        if t.kind == TokKind::Word {
            let word = token_text(sql, t).to_lowercase();
            if word == "with" {
                i += 1;
                collect_ctes(sql, stmt_tokens, &mut i, &mut relations);
                continue;
            }
            if RELATION_KEYWORDS.contains(&word.as_str()) {
                i += 1;
                // `FROM`/comma-joined relations repeat; JOIN/INTO/UPDATE/TABLE take just one.
                loop {
                    let Some(rel) = parse_relation(sql, stmt_tokens, &mut i) else { break };
                    relations.push(rel);
                    if word == "from" && stmt_tokens.get(i).is_some_and(|t| t.kind == TokKind::Comma) {
                        i += 1;
                        continue;
                    }
                    break;
                }
                continue;
            }
        }
        i += 1;
    }
    // A later bare reference to a CTE name (`from recent r`) is parsed like any other relation,
    // but it should resolve against the CTE's body, not a real table called "recent".
    let ctes: Vec<(String, Option<(usize, usize)>)> =
        relations.iter().filter(|r| r.is_cte).map(|r| (r.name.clone(), r.cte_body)).collect();
    for rel in relations.iter_mut().filter(|r| !r.is_cte && r.schema.is_none()) {
        if let Some((_, body)) = ctes.iter().find(|(name, _)| eq_ident(name, &rel.name)) {
            rel.is_cte = true;
            rel.cte_body = *body;
        }
    }
    relations
}

/// `WITH a AS (...), b AS (...)`: registers each CTE name and skips its parenthesized body.
fn collect_ctes(sql: &str, stmt_tokens: &[&Token], i: &mut usize, relations: &mut Vec<Relation>) {
    loop {
        let Some(name_tok) = stmt_tokens.get(*i).filter(|t| is_name(t)) else { break };
        let name = dequote(sql, name_tok);
        *i += 1;
        // Optional column list: `a (x, y) AS (...)`.
        if stmt_tokens.get(*i).is_some_and(|t| t.kind == TokKind::OpenParen) {
            skip_parens(stmt_tokens, i);
        }
        if !stmt_tokens.get(*i).is_some_and(|t| token_text(sql, t).eq_ignore_ascii_case("as")) {
            break;
        }
        *i += 1;
        let mut body = None;
        if stmt_tokens.get(*i).is_some_and(|t| t.kind == TokKind::OpenParen) {
            let open = stmt_tokens[*i];
            skip_parens(stmt_tokens, i);
            // `*i` now points right after the matching close paren.
            if let Some(close) = stmt_tokens.get(*i - 1).filter(|t| t.kind == TokKind::CloseParen) {
                if close.start > open.end {
                    body = Some((open.end, close.start));
                }
            }
        }
        relations.push(Relation { schema: None, name, alias: None, is_cte: true, cte_body: body });
        if stmt_tokens.get(*i).is_some_and(|t| t.kind == TokKind::Comma) {
            *i += 1;
            continue;
        }
        break;
    }
}

fn skip_parens(stmt_tokens: &[&Token], i: &mut usize) {
    let mut depth = 0;
    while *i < stmt_tokens.len() {
        match stmt_tokens[*i].kind {
            TokKind::OpenParen => depth += 1,
            TokKind::CloseParen => depth -= 1,
            _ => {}
        }
        *i += 1;
        if depth == 0 {
            break;
        }
    }
}

const CLAUSE_WORDS: &[&str] = &[
    "where", "on", "join", "inner", "left", "right", "full", "cross", "group", "order", "having", "limit", "offset",
    "set", "values", "union", "as", "using",
];

/// `[schema.]name [[AS] alias]` starting at `*i`, advancing past it. `None` if there's no name.
fn parse_relation(sql: &str, stmt_tokens: &[&Token], i: &mut usize) -> Option<Relation> {
    let first = stmt_tokens.get(*i).filter(|t| is_name(t))?;
    let mut schema = None;
    let mut name = dequote(sql, first);
    *i += 1;
    if stmt_tokens.get(*i).is_some_and(|t| t.kind == TokKind::Dot) {
        if let Some(second) = stmt_tokens.get(*i + 1).filter(|t| is_name(t)) {
            schema = Some(name);
            name = dequote(sql, second);
            *i += 2;
        }
    }
    // Optional subquery/function call args right after the name: not a relation we can resolve.
    if stmt_tokens.get(*i).is_some_and(|t| t.kind == TokKind::OpenParen) {
        skip_parens(stmt_tokens, i);
    }
    let mut alias = None;
    if let Some(t) = stmt_tokens.get(*i).filter(|t| t.kind == TokKind::Word && token_text(sql, t).eq_ignore_ascii_case("as")) {
        let _ = t;
        *i += 1;
        if let Some(a) = stmt_tokens.get(*i).filter(|t| is_name(t)) {
            alias = Some(dequote(sql, a));
            *i += 1;
        }
    } else if let Some(a) = stmt_tokens.get(*i).filter(|t| {
        is_name(t) && !(t.kind == TokKind::Word && CLAUSE_WORDS.contains(&token_text(sql, t).to_lowercase().as_str()))
    }) {
        alias = Some(dequote(sql, a));
        *i += 1;
    }
    Some(Relation { schema, name, alias, is_cte: false, cte_body: None })
}

/// A cap on how deep `WITH` CTEs can reference each other while resolving columns: real scripts
/// never nest this far, and it keeps the heuristic parser from ever looping.
const MAX_CTE_DEPTH: u8 = 3;

enum SelectItem {
    /// `*` or `t.*`: all columns of the statement's relations (approximated: every relation's
    /// columns, not just `t`'s \u2014 good enough for completion).
    Star,
    Named(String),
    /// An expression with no name we can give it (e.g. `price * qty` with no `AS`).
    Unnamed,
}

/// The `SELECT` list of the first statement in `tokens` (e.g. a CTE's body), split on top-level
/// commas. Stops at the matching `FROM`, so it also works for `SELECT 1, 2` with no `FROM`.
fn select_list_items(sql: &str, tokens: &[&Token]) -> Vec<SelectItem> {
    let Some(select_idx) =
        tokens.iter().position(|t| t.kind == TokKind::Word && token_text(sql, t).eq_ignore_ascii_case("select"))
    else {
        return Vec::new();
    };
    let mut i = select_idx + 1;
    if tokens.get(i).is_some_and(|t| token_text(sql, t).eq_ignore_ascii_case("distinct")) {
        i += 1;
    }
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut item_start = i;
    let mut j = i;
    while j < tokens.len() {
        match tokens[j].kind {
            TokKind::OpenParen => depth += 1,
            TokKind::CloseParen => depth -= 1,
            TokKind::Word if depth == 0 && token_text(sql, tokens[j]).eq_ignore_ascii_case("from") => break,
            TokKind::Comma if depth == 0 => {
                items.push(parse_select_item(sql, &tokens[item_start..j]));
                item_start = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    if item_start < j {
        items.push(parse_select_item(sql, &tokens[item_start..j]));
    }
    items
}

/// Only recognizes the unambiguous cases: a bare (optionally qualified) column, or an explicit
/// `AS alias`. An expression without `AS` (`price * qty`) is `Unnamed` rather than guessed at,
/// since a bare trailing identifier there is as likely to be part of the expression as an alias.
fn parse_select_item(sql: &str, item_tokens: &[&Token]) -> SelectItem {
    if item_tokens.last().is_some_and(|t| t.kind == TokKind::Other && token_text(sql, t) == "*") {
        return SelectItem::Star;
    }
    if item_tokens.len() >= 2 {
        let as_idx = item_tokens.len() - 2;
        if token_text(sql, item_tokens[as_idx]).eq_ignore_ascii_case("as") && is_name(item_tokens[item_tokens.len() - 1]) {
            return SelectItem::Named(dequote(sql, item_tokens[item_tokens.len() - 1]));
        }
    }
    if let [name] = item_tokens {
        if is_name(name) {
            return SelectItem::Named(dequote(sql, name));
        }
    }
    if let [_, dot, name] = item_tokens {
        if dot.kind == TokKind::Dot && is_name(name) {
            return SelectItem::Named(dequote(sql, name));
        }
    }
    SelectItem::Unnamed
}

/// Columns a CTE's body projects, by re-running the same heuristics one level in. Recurses for
/// `SELECT *` over another CTE, bounded by `MAX_CTE_DEPTH`.
fn cte_columns(sql: &str, body: (usize, usize), catalog: &Catalog, depth: u8) -> Vec<CompletionItem> {
    if depth >= MAX_CTE_DEPTH || body.0 >= body.1 {
        return Vec::new();
    }
    let body_sql = &sql[body.0..body.1];
    let tokens = tokenize(body_sql);
    let token_refs: Vec<&Token> = tokens.iter().collect();
    let relations = collect_relations(body_sql, &token_refs);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in select_list_items(body_sql, &token_refs) {
        match item {
            SelectItem::Star => {
                for rel in &relations {
                    for c in table_columns(body_sql, rel, catalog, depth + 1) {
                        if seen.insert(c.label.clone()) {
                            out.push(c);
                        }
                    }
                }
            }
            SelectItem::Named(name) if seen.insert(name.clone()) => {
                out.push(CompletionItem { label: name.clone(), insert_text: name, kind: CompletionKind::Column, detail: None });
            }
            SelectItem::Named(_) | SelectItem::Unnamed => {}
        }
    }
    out
}

// MARK: Candidates

fn relation_candidates(catalog: &Catalog) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = catalog
        .schemas
        .iter()
        .map(|s| CompletionItem { label: s.name.clone(), insert_text: s.name.clone(), kind: CompletionKind::Schema, detail: None })
        .collect();
    items.extend(catalog.tables(None).map(|(schema, t)| CompletionItem {
        label: t.name.clone(),
        insert_text: t.name.clone(),
        kind: if t.kind == TableKind::View { CompletionKind::View } else { CompletionKind::Table },
        detail: Some(schema.to_string()),
    }));
    items
}

fn qualified_candidates(sql: &str, qualifier: &str, relations: &[Relation], catalog: &Catalog, depth: u8) -> Vec<CompletionItem> {
    // An alias or table name wins over a schema of the same name (more specific), but only when
    // it actually resolves to something: `from public.` parses "public" as a bare relation name,
    // which should still fall back to a schema lookup.
    if let Some(rel) = relations.iter().find(|r| r.alias.as_deref().is_some_and(|a| eq_ident(a, qualifier))) {
        return table_columns(sql, rel, catalog, depth);
    }
    if let Some(rel) = relations.iter().find(|r| eq_ident(&r.name, qualifier)) {
        let columns = table_columns(sql, rel, catalog, depth);
        if !columns.is_empty() || rel.is_cte {
            return columns;
        }
    }
    if let Some(table) = catalog.find_table(qualifier) {
        return columns_of(table);
    }
    if let Some(schema) = catalog.find_schema(qualifier) {
        return schema
            .tables
            .iter()
            .map(|t| CompletionItem {
                label: t.name.clone(),
                insert_text: t.name.clone(),
                kind: if t.kind == TableKind::View { CompletionKind::View } else { CompletionKind::Table },
                detail: Some(schema.name.clone()),
            })
            .collect();
    }
    Vec::new()
}

fn table_columns(sql: &str, rel: &Relation, catalog: &Catalog, depth: u8) -> Vec<CompletionItem> {
    if rel.is_cte {
        return rel.cte_body.map(|body| cte_columns(sql, body, catalog, depth)).unwrap_or_default();
    }
    let table = match &rel.schema {
        Some(schema) => catalog.tables(Some(schema)).map(|(_, t)| t).find(|t| eq_ident(&t.name, &rel.name)),
        None => catalog.find_table(&rel.name),
    };
    table.map(columns_of).unwrap_or_default()
}

fn columns_of(table: &CatalogTable) -> Vec<CompletionItem> {
    table
        .columns
        .iter()
        .map(|c| CompletionItem {
            label: c.name.clone(),
            insert_text: c.name.clone(),
            kind: CompletionKind::Column,
            detail: Some(c.type_name.clone()),
        })
        .collect()
}

fn column_candidates(sql: &str, relations: &[Relation], catalog: &Catalog, dialect: Dialect, depth: u8) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for rel in relations {
        items.extend(table_columns(sql, rel, catalog, depth));
    }
    items.extend(function_items(dialect));
    items.extend(keyword_items());
    items
}

fn statement_start_candidates() -> Vec<CompletionItem> {
    const STARTERS: &[&str] = &["select", "insert into", "update", "delete from", "create table", "create view",
        "create index", "alter table", "drop table", "with"];
    STARTERS
        .iter()
        .map(|k| CompletionItem { label: (*k).into(), insert_text: (*k).into(), kind: CompletionKind::Keyword, detail: None })
        .collect()
}

fn keyword_items() -> Vec<CompletionItem> {
    KEYWORDS
        .iter()
        .map(|k| CompletionItem { label: (*k).into(), insert_text: (*k).into(), kind: CompletionKind::Keyword, detail: None })
        .collect()
}

fn function_items(dialect: Dialect) -> Vec<CompletionItem> {
    dialect_functions(dialect)
        .iter()
        .map(|f| CompletionItem { label: (*f).into(), insert_text: format!("{f}("), kind: CompletionKind::Function, detail: None })
        .collect()
}

fn dialect_functions(dialect: Dialect) -> Vec<&'static str> {
    const COMMON: &[&str] = &["count", "sum", "avg", "min", "max", "coalesce", "nullif", "cast", "lower", "upper",
        "trim", "length", "substring", "concat", "round", "abs", "now"];
    let specific: &[&str] = match dialect.0 {
        DatabaseKind::Postgres => {
            &["date_trunc", "extract", "array_agg", "json_agg", "jsonb_build_object", "to_char", "generate_series"]
        }
        DatabaseKind::Mysql => &["if", "ifnull", "date_format", "group_concat", "json_extract", "curdate"],
        DatabaseKind::Sqlite | DatabaseKind::Libsql | DatabaseKind::D1 => &["strftime", "ifnull", "json_extract", "group_concat", "printf"],
        DatabaseKind::SqlServer => &["getdate", "sysdatetime", "isnull", "convert", "try_cast", "dateadd", "datediff",
            "datepart", "format", "len", "charindex", "newid", "iif", "string_agg", "json_value"],
    };
    COMMON.iter().chain(specific).copied().collect()
}

// MARK: Ranking

fn filter_and_rank(items: &mut Vec<CompletionItem>, prefix: &str) {
    items.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    items.dedup_by(|a, b| a.label.eq_ignore_ascii_case(&b.label) && a.kind == b.kind);
    if prefix.is_empty() {
        items.sort_by_key(kind_rank);
        return;
    }
    items.retain(|i| i.label.to_lowercase().starts_with(prefix));
    items.sort_by(|a, b| kind_rank(a).cmp(&kind_rank(b)).then(a.label.len().cmp(&b.label.len())));
}

fn kind_rank(item: &CompletionItem) -> u8 {
    match item.kind {
        CompletionKind::Column => 0,
        CompletionKind::Table | CompletionKind::View => 1,
        CompletionKind::Schema => 2,
        CompletionKind::Function => 3,
        CompletionKind::Keyword => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, TableInfo};

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), type_name: ty.into(), is_primary_key: false, is_nullable: false }
    }

    fn catalog() -> Catalog {
        let schemas = vec![
            Schema {
                name: "public".into(),
                tables: vec![TableInfo::new("public", "users"), TableInfo::new("public", "orders")],
            },
            Schema { name: "billing".into(), tables: vec![TableInfo::new("billing", "invoices")] },
        ];
        let columns = vec![
            TableColumns {
                schema: "public".into(),
                table: "users".into(),
                columns: vec![col("id", "bigint"), col("name", "text"), col("email", "text")],
            },
            TableColumns {
                schema: "public".into(),
                table: "orders".into(),
                columns: vec![col("id", "bigint"), col("user_id", "bigint"), col("total", "numeric")],
            },
            TableColumns {
                schema: "billing".into(),
                table: "invoices".into(),
                columns: vec![col("id", "bigint"), col("amount", "numeric")],
            },
        ];
        Catalog::new(schemas, columns)
    }

    fn labels(c: &Completions) -> Vec<&str> {
        c.items.iter().map(|i| i.label.as_str()).collect()
    }

    fn pg() -> Dialect {
        Dialect(DatabaseKind::Postgres)
    }

    #[test]
    fn columns_after_select_before_from() {
        let sql = "select  from users";
        let offset = sql.find("  ").unwrap() + 1;
        let c = complete(sql, offset, &catalog(), pg());
        assert!(labels(&c).contains(&"id"), "{:?}", labels(&c));
        assert!(labels(&c).contains(&"name"));
        assert!(labels(&c).contains(&"email"));
        assert!(!labels(&c).contains(&"amount"));
    }

    #[test]
    fn qualified_columns_via_alias() {
        let sql = "select u. from users u";
        let offset = sql.find("u.").unwrap() + 2;
        let c = complete(sql, offset, &catalog(), pg());
        assert_eq!(labels(&c), vec!["email", "id", "name"]);
    }

    #[test]
    fn join_with_alias() {
        let sql = "select o. from users u join orders o on o.user_id = u.id";
        let offset = sql.find("o. from").unwrap() + 2;
        let c = complete(sql, offset, &catalog(), pg());
        assert_eq!(labels(&c), vec!["id", "total", "user_id"]);
    }

    #[test]
    fn tables_after_from_with_prefix() {
        let sql = "select id from ord";
        let c = complete(sql, sql.len(), &catalog(), pg());
        assert_eq!(labels(&c), vec!["orders"]);
    }

    #[test]
    fn schema_qualified_tables() {
        let sql = "select * from public.";
        let c = complete(sql, sql.len(), &catalog(), pg());
        assert_eq!(labels(&c), vec!["orders", "users"]);
    }

    #[test]
    fn cte_with_wildcard_resolves_the_inner_tables_columns() {
        let sql = "with recent as (select * from orders) select  from recent";
        let offset = sql.find("select  from").unwrap() + 7;
        let c = complete(sql, offset, &catalog(), pg());
        assert!(labels(&c).contains(&"id"));
        assert!(labels(&c).contains(&"total"));
        assert!(labels(&c).contains(&"coalesce"));
    }

    #[test]
    fn cte_with_explicit_columns_resolves_just_those() {
        let sql = "with recent as (select id, total as amount from orders) select r. from recent r";
        let offset = sql.find("r. from").unwrap() + 2;
        let c = complete(sql, offset, &catalog(), pg());
        assert_eq!(labels(&c), vec!["amount", "id"]);
    }

    #[test]
    fn cte_with_unnamed_expression_skips_that_column() {
        let sql = "with recent as (select id, total * 2 from orders) select r. from recent r";
        let offset = sql.find("r. from").unwrap() + 2;
        let c = complete(sql, offset, &catalog(), pg());
        assert_eq!(labels(&c), vec!["id"]);
    }

    #[test]
    fn ambiguous_table_name_prefers_public_schema() {
        let schemas = vec![
            Schema { name: "public".into(), tables: vec![TableInfo::new("public", "logs")] },
            Schema { name: "archive".into(), tables: vec![TableInfo::new("archive", "logs")] },
        ];
        let columns = vec![
            TableColumns { schema: "public".into(), table: "logs".into(), columns: vec![col("id", "bigint")] },
            TableColumns { schema: "archive".into(), table: "logs".into(), columns: vec![col("archived_at", "timestamptz")] },
        ];
        let cat = Catalog::new(schemas, columns);
        let sql = "select l. from logs l";
        let offset = sql.find("l. from").unwrap() + 2;
        let c = complete(sql, offset, &cat, pg());
        assert_eq!(labels(&c), vec!["id"]);
    }

    #[test]
    fn unfinished_where_clause_offers_columns() {
        let sql = "select id from users where ";
        let c = complete(sql, sql.len(), &catalog(), pg());
        assert!(labels(&c).contains(&"id"));
        assert!(labels(&c).contains(&"name"));
    }

    #[test]
    fn multiple_statements_scope_to_the_current_one() {
        let sql = "select 1; select  from users";
        let offset = sql.rfind("select  from").unwrap() + 7;
        let c = complete(sql, offset, &catalog(), pg());
        assert!(labels(&c).contains(&"id"));
        assert!(labels(&c).contains(&"name"));
    }

    #[test]
    fn non_ascii_text_does_not_panic() {
        let sql = "select 'ñandú 🐻' as año from users where ";
        let _ = complete(sql, sql.len(), &catalog(), pg());
        let _ = complete(sql, sql.find("año").unwrap(), &catalog(), pg());
    }

    #[test]
    fn quoted_identifiers_resolve_to_columns() {
        let sql = r#"select u. from "public"."users" u"#;
        let offset = sql.find("u.").unwrap() + 2;
        let c = complete(sql, offset, &catalog(), pg());
        assert_eq!(labels(&c), vec!["email", "id", "name"]);
    }

    #[test]
    fn comma_joined_relations() {
        let sql = "select  from users u, orders o";
        let offset = sql.find("  ").unwrap() + 1;
        let c = complete(sql, offset, &catalog(), pg());
        assert!(labels(&c).contains(&"id"));
        assert!(labels(&c).contains(&"total"));
        assert!(labels(&c).contains(&"name"));
    }

    #[test]
    fn statement_start_offers_statement_keywords() {
        let sql = "";
        let c = complete(sql, 0, &catalog(), pg());
        assert!(labels(&c).contains(&"select"));
    }

    #[test]
    fn empty_catalog_does_not_panic() {
        let c = complete("select * from ", 14, &Catalog::default(), pg());
        assert!(c.items.is_empty() || c.items.iter().all(|i| i.kind == CompletionKind::Keyword));
    }

    #[test]
    fn completes_inside_a_table_filter() {
        let catalog = Catalog::new(
            vec![Schema { name: "public".into(), tables: vec![TableInfo::new("public", "orders"), TableInfo::new("public", "users")] }],
            vec![
                TableColumns { schema: "public".into(), table: "orders".into(), columns: vec![col("id", "text"), col("status", "text"), col("total", "text")] },
                TableColumns { schema: "public".into(), table: "users".into(), columns: vec![col("email", "text"), col("status_code", "text")] },
            ],
        );
        let pg = Dialect(DatabaseKind::Postgres);
        let filter = "total > 10 and sta";
        let result = complete_filter(filter, filter.len(), &catalog, pg, "public", "orders");
        // The range points into the filter, not the generated query around it.
        assert_eq!(&filter[result.replace_start..result.replace_end], "sta");
        let labels: Vec<_> = result.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels.first(), Some(&"status"), "{labels:?}");
        assert!(!labels.contains(&"status_code"), "other tables' columns aren't offered: {labels:?}");

        let empty = complete_filter("", 0, &catalog, pg, "public", "orders");
        assert_eq!((empty.replace_start, empty.replace_end), (0, 0));
        assert!(empty.items.iter().any(|i| i.label == "total"));
    }
}
