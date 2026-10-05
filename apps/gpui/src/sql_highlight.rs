//! SQL highlighting for the script editor, from the core's tree-sitter highlighter
//! (`dbcore::highlight`), so it matches the macOS editor.

use std::ops::Range;
use std::rc::Rc;

use dbcore::highlight::{HighlightKind, highlight_sql};
use gpui_kit::component::input::{FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, InputHighlighterFactory, Rope};
use gpui_kit::*;

pub const LANGUAGE: &str = "sql";

/// Installs on an editor with `.set_highlighter_factory(factory(), cx)`.
pub fn factory() -> InputHighlighterFactory {
    Rc::new(|language: &str| (language == LANGUAGE).then(|| Box::new(SqlHighlighter::default()) as Box<dyn InputHighlighter>))
}

#[derive(Default)]
struct SqlHighlighter {
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

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        LANGUAGE.into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        // Scripts are small enough to re-highlight whole (the macOS editor does the same).
        let source = text.to_string();
        let mut spans: Vec<(Range<usize>, &'static str)> =
            highlight_sql(&source).into_iter().map(|s| (s.start..s.end, theme_name(s.kind))).collect();
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
