//! Syntax highlighting for the editors (SQL scripts, JSON in the inspector), from the core's
//! tree-sitter highlighters (`dbcore::highlight`), so it matches the macOS app.

use std::ops::Range;
use std::rc::Rc;

use dbcore::highlight::{HighlightKind, HighlightSpan, highlight_json, highlight_sql};
use gpui_kit::component::input::{FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, InputHighlighterFactory, Rope};
use gpui_kit::*;

pub const SQL: &str = "sql";
pub const JSON: &str = "json";
/// No highlighting.
pub const PLAIN: &str = "text";

/// Installs on an editor with `.set_highlighter_factory(factory(), cx)`; the editor's language
/// (`SQL`, `JSON`) picks the highlighter.
pub fn factory() -> InputHighlighterFactory {
    Rc::new(|language: &str| {
        let highlight: fn(&str) -> Vec<HighlightSpan> = match language {
            SQL => highlight_sql,
            JSON => highlight_json,
            _ => return None,
        };
        Some(Box::new(Highlighter { language: language.to_string().into(), highlight, spans: Vec::new() }) as Box<dyn InputHighlighter>)
    })
}

struct Highlighter {
    language: SharedString,
    highlight: fn(&str) -> Vec<HighlightSpan>,
    /// Byte ranges and theme names, sorted and non-overlapping (as the core returns them).
    spans: Vec<(Range<usize>, &'static str)>,
}

/// The theme's syntax names (see gpui-component's highlight registry).
fn theme_name(kind: HighlightKind) -> &'static str {
    match kind {
        HighlightKind::Keyword => "keyword",
        HighlightKind::Type => "type",
        HighlightKind::Object => "tag",
        HighlightKind::Function => "function",
        HighlightKind::Field => "property",
        HighlightKind::Variable => "variable",
        HighlightKind::Parameter => "variable.special",
        HighlightKind::String => "string",
        HighlightKind::Number => "number",
        HighlightKind::Constant => "boolean",
        HighlightKind::Comment => "comment",
        HighlightKind::Operator => "operator",
        HighlightKind::Punctuation => "punctuation",
    }
}

impl InputHighlighter for Highlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        // Scripts and values are small enough to re-highlight whole (the macOS app does the same).
        let source = text.to_string();
        let mut spans: Vec<(Range<usize>, &'static str)> =
            (self.highlight)(&source).into_iter().map(|s| (s.start..s.end, theme_name(s.kind))).collect();
        spans.sort_by_key(|(range, _)| range.start);
        let mut end = 0;
        spans.retain(|(range, _)| {
            let keep = range.start >= end && range.start < range.end;
            if keep {
                end = range.end;
            }
            keep
        });
        self.spans = spans;
    }

    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        // Runs must cover `range` completely: fill the gaps with the default style.
        let mut out = Vec::new();
        let mut at = range.start;
        let first = self.spans.partition_point(|(r, _)| r.end <= range.start);
        for (span, name) in &self.spans[first..] {
            if span.start >= range.end {
                break;
            }
            let (start, end) = (span.start.max(range.start), span.end.min(range.end));
            if start > at {
                out.push((at..start, HighlightStyle::default()));
            }
            if end > start {
                out.push((start..end, resolver.style(name).unwrap_or_default()));
                at = end;
            }
        }
        if at < range.end {
            out.push((at..range.end, HighlightStyle::default()));
        }
        out
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

use gpui_kit::component::input::EditorState;
