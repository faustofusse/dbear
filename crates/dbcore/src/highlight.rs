//! Syntax highlighting with tree-sitter: SQL (grammar: github.com/DerekStride/tree-sitter-sql)
//! for the editor, JSON for the value inspector.
//!
//! Frontends get flat, sorted spans and only decide colors. Offsets are UTF-8 byte offsets;
//! the FFI layer converts them to UTF-16 for AppKit.

use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    /// Built-in data types (`int`, `text`, `timestamptz`…).
    Type,
    /// Tables, views, schemas: names of database objects.
    Object,
    Function,
    Field,
    /// Aliases.
    Variable,
    Parameter,
    String,
    Number,
    /// `true` / `false`.
    Constant,
    Comment,
    Operator,
    Punctuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

struct Highlighter {
    language: tree_sitter::Language,
    query: Query,
    /// Capture index -> kind (`None` for captures we ignore, like `@spell`).
    kinds: Vec<Option<HighlightKind>>,
}

/// Additions to the grammar's `highlights.scm`. Appended, so they win on identical ranges.
const EXTRA_QUERY: &str = r#"
(object_reference schema: (identifier) @type)
(object_reference database: (identifier) @type)
; `interval '90 days'`: the quoted part is an anonymous token, so color the whole node;
; the nested keyword span still overrides the `interval` word.
(interval) @string
"#;

/// JSON captures: our own query rather than the grammar's, so keys and punctuation get kinds too.
/// The key pattern comes after `(string)`, so it wins on the key's range.
const JSON_QUERY: &str = r#"
(string) @string
(pair key: (string) @field)
(number) @number
[(true) (false) (null)] @boolean
(comment) @comment
["{" "}" "[" "]"] @punctuation.bracket
[":" ","] @punctuation.delimiter
"#;

impl Highlighter {
    fn new(language: tree_sitter::Language, source: &str) -> Self {
        let query = Query::new(&language, source).expect("bundled highlight query must compile");
        let kinds = query.capture_names().iter().map(|name| kind_for_capture(name)).collect();
        Highlighter { language, query, kinds }
    }
}

fn sql_highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let source = format!("{}\n{EXTRA_QUERY}", tree_sitter_sequel::HIGHLIGHTS_QUERY);
        Highlighter::new(tree_sitter_sequel::LANGUAGE.into(), &source)
    })
}

fn json_highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| Highlighter::new(tree_sitter_json::LANGUAGE.into(), JSON_QUERY))
}

fn kind_for_capture(name: &str) -> Option<HighlightKind> {
    use HighlightKind::*;
    Some(match name {
        "keyword" | "keyword.operator" | "conditional" | "storageclass" | "type.qualifier" | "attribute" => Keyword,
        "type.builtin" => Type,
        "type" => Object,
        "function.call" => Function,
        "field" => Field,
        "variable" => Variable,
        "parameter" => Parameter,
        // The SQL grammar's number patterns use Lua-style `%d` (Neovim) and never match here,
        // so SQL literals are classified from their text instead (see `classify_literal`).
        "string" => String,
        "number" | "float" => Number,
        "boolean" => Constant,
        "comment" => Comment,
        "operator" => Operator,
        "punctuation.bracket" | "punctuation.delimiter" => Punctuation,
        _ => return None,
    })
}

fn classify_literal(text: &str) -> HighlightKind {
    let t = text.trim_start_matches(['-', '+']);
    let numeric = !t.is_empty()
        && t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.')
        && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '_' | '-' | '+'));
    if numeric { HighlightKind::Number } else { HighlightKind::String }
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

/// Highlight spans for a SQL script, sorted by start. Nested spans come after their parent,
/// so applying them in order lets the most specific one win.
pub fn highlight_sql(source: &str) -> Vec<HighlightSpan> {
    highlight(sql_highlighter(), source)
}

/// Highlight spans for a JSON value: keys are `Field`, `true`/`false`/`null` are `Constant`.
/// Invalid JSON (e.g. while editing) still highlights the parts that parse.
pub fn highlight_json(source: &str) -> Vec<HighlightSpan> {
    highlight(json_highlighter(), source)
}

fn highlight(hl: &Highlighter, source: &str) -> Vec<HighlightSpan> {
    let Some(tree) = PARSER.with_borrow_mut(|parser| {
        parser.set_language(&hl.language).expect("tree-sitter grammar ABI must be supported");
        parser.parse(source, None)
    }) else {
        return Vec::new();
    };

    // (start, end, pattern index, kind). Several patterns can capture the same node
    // (e.g. a function name is also an object reference): the later pattern wins, as in Neovim.
    let mut spans: Vec<(usize, usize, usize, HighlightKind)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&hl.query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        for capture in m.captures() {
            let Some(mut kind) = hl.kinds[capture.index as usize] else { continue };
            let range = capture.node.byte_range();
            if range.is_empty() {
                continue;
            }
            if kind == HighlightKind::String && capture.node.kind() == "literal" {
                kind = classify_literal(&source[range.clone()]);
            }
            spans.push((range.start, range.end, m.pattern_index, kind));
        }
    }

    // Outer spans first, then inner; for identical ranges the highest pattern index last.
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let mut out: Vec<HighlightSpan> = Vec::with_capacity(spans.len());
    for (start, end, _, kind) in spans {
        match out.last_mut() {
            Some(last) if last.start == start && last.end == end => last.kind = kind,
            _ => out.push(HighlightSpan { start, end, kind }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use HighlightKind::*;

    fn kinds(sql: &str) -> Vec<(&str, HighlightKind)> {
        highlight_sql(sql).into_iter().map(|s| (&sql[s.start..s.end], s.kind)).collect()
    }

    #[test]
    fn highlights_basic_select() {
        let k = kinds("select id, name from public.users where id = 42 and name = 'ada' -- hi");
        assert!(k.contains(&("select", Keyword)));
        assert!(k.contains(&("from", Keyword)));
        assert!(k.contains(&("users", Object)));
        assert!(k.contains(&("public", Object)));
        assert!(k.contains(&("42", Number)));
        assert!(k.contains(&("'ada'", String)));
        assert!(k.contains(&("-- hi", Comment)));
    }

    #[test]
    fn function_call_beats_object_reference() {
        let k = kinds("select count(*) from orders");
        assert!(k.contains(&("count", Function)), "{k:?}");
        assert!(!k.contains(&("count", Object)));
    }

    #[test]
    fn interval_literal_is_a_string() {
        let sql = "select now() - interval '90 days'";
        let spans = highlight_sql(sql);
        let at = sql.find("'90").unwrap();
        // The innermost span covering the quote decides its color.
        let kind = spans.iter().rev().find(|s| s.start <= at && at < s.end).map(|s| s.kind);
        assert_eq!(kind, Some(String));
        assert!(kinds(sql).iter().any(|(t, k)| *t == "interval" && *k != String));
    }

    #[test]
    fn types_and_booleans() {
        let k = kinds("create table t (id bigint, ok boolean default true)");
        assert!(k.contains(&("bigint", Type)), "{k:?}");
        assert!(k.contains(&("true", Constant)), "{k:?}");
    }

    #[test]
    fn spans_are_sorted_and_on_char_boundaries() {
        let sql = "select 'ñandú 🐻' as \"año\", 3.14 from t where x <> 1;\nselect 1";
        let spans = highlight_sql(sql);
        assert!(spans.windows(2).all(|w| w[0].start <= w[1].start));
        assert!(spans.iter().all(|s| sql.is_char_boundary(s.start) && sql.is_char_boundary(s.end)));
        assert!(kinds(sql).contains(&("3.14", Number)));
    }

    fn json_kinds(json: &str) -> Vec<(&str, HighlightKind)> {
        highlight_json(json).into_iter().map(|s| (&json[s.start..s.end], s.kind)).collect()
    }

    #[test]
    fn highlights_json_keys_and_values() {
        let k = json_kinds(r#"{"id": "21", "n": -1.5e3, "ok": true, "x": null, "tags": ["ñandú 🐻"]}"#);
        assert!(k.contains(&(r#""id""#, Field)), "{k:?}");
        assert!(!k.contains(&(r#""id""#, String)));
        assert!(k.contains(&(r#""21""#, String)));
        assert!(k.contains(&("-1.5e3", Number)));
        assert!(k.contains(&("true", Constant)));
        assert!(k.contains(&("null", Constant)));
        assert!(k.contains(&(r#""ñandú 🐻""#, String)));
        assert!(k.contains(&("{", Punctuation)));
        assert!(k.contains(&(":", Punctuation)));
    }

    #[test]
    fn broken_json_still_highlights() {
        let k = json_kinds(r#"{"a": 1, "b": "#);
        assert!(k.contains(&(r#""a""#, Field)), "{k:?}");
        assert!(k.contains(&("1", Number)));
    }

    #[test]
    fn sql_and_json_share_a_thread() {
        assert!(kinds("select 1").contains(&("select", Keyword)));
        assert!(json_kinds("[1]").contains(&("1", Number)));
        assert!(kinds("select 1").contains(&("select", Keyword)));
    }

    #[test]
    fn broken_sql_still_highlights_and_empty_is_empty() {
        assert!(highlight_sql("").is_empty());
        let k = kinds("selec * frm users where");
        assert!(k.iter().any(|(_, kind)| *kind == Keyword) || k.is_empty());
    }
}
