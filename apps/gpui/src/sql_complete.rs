//! SQL completion for script editors, from the core's completer (`dbcore::complete`), so it suggests
//! the same keywords, tables and columns as the macOS editor.

use std::cell::RefCell;
use std::rc::Rc;

use dbcore::complete::{Catalog, CompletionKind, complete};
use dbcore::dialect::Dialect;
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};

/// The catalog arrives after the editor exists (it's loaded from the server), hence the cell.
pub type SharedCatalog = Rc<RefCell<Option<Rc<Catalog>>>>;

pub struct SqlCompletion {
    pub catalog: SharedCatalog,
    pub dialect: Dialect,
}

fn kind(kind: CompletionKind) -> CompletionItemKind {
    match kind {
        CompletionKind::Keyword => CompletionItemKind::KEYWORD,
        CompletionKind::Schema => CompletionItemKind::MODULE,
        CompletionKind::Table => CompletionItemKind::STRUCT,
        CompletionKind::View => CompletionItemKind::INTERFACE,
        CompletionKind::Column => CompletionItemKind::FIELD,
        CompletionKind::Function => CompletionItemKind::FUNCTION,
    }
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        // Keywords still complete before the catalog has loaded.
        let catalog = self.catalog.borrow().clone().unwrap_or_else(|| Rc::new(Catalog::new(Vec::new(), Vec::new())));
        let sql = text.to_string();
        let found = complete(&sql, offset, &catalog, self.dialect);
        let range = lsp_types::Range {
            start: text.offset_to_position(found.replace_start),
            end: text.offset_to_position(found.replace_end),
        };
        let items = found
            .items
            .into_iter()
            .map(|item| CompletionItem {
                label: item.label,
                kind: Some(kind(item.kind)),
                detail: item.detail,
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: item.insert_text })),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        // While typing a name, or right after `schema.`.
        new_text.chars().any(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
