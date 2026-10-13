//! The command palette's search (`dbcore::palette`). The app hands over everything the palette
//! lists once, when it opens, and ranks it on each keystroke.

use std::sync::Arc;

use dbcore::palette as core;

#[derive(uniffi::Enum, Clone, Copy)]
pub enum PaletteKind {
    Command,
    Table,
    Connection,
    Tab,
}

impl From<PaletteKind> for core::Kind {
    fn from(kind: PaletteKind) -> Self {
        match kind {
            PaletteKind::Command => core::Kind::Command,
            PaletteKind::Table => core::Kind::Table,
            PaletteKind::Connection => core::Kind::Connection,
            PaletteKind::Tab => core::Kind::Tab,
        }
    }
}

#[derive(uniffi::Record)]
pub struct PaletteCandidate {
    pub kind: PaletteKind,
    pub label: String,
    pub keywords: Vec<String>,
}

#[derive(uniffi::Record)]
pub struct PaletteMatch {
    /// Index into the candidates the palette was made with.
    pub index: u32,
    pub score: i32,
    /// UTF-16 offsets of the label's matched characters, ascending.
    pub positions: Vec<u32>,
}

/// What an open palette lists.
#[derive(uniffi::Object)]
pub struct Palette {
    candidates: Vec<core::Candidate>,
}

#[uniffi::export]
impl Palette {
    #[uniffi::constructor]
    pub fn new(candidates: Vec<PaletteCandidate>) -> Arc<Self> {
        let candidates = candidates
            .into_iter()
            .map(|c| core::Candidate { kind: c.kind.into(), label: c.label, keywords: c.keywords })
            .collect();
        Arc::new(Self { candidates })
    }

    /// The candidates matching `query`, best first, at most `limit`.
    pub fn rank(&self, query: String, limit: u32) -> Vec<PaletteMatch> {
        core::rank(&query, &self.candidates, limit as usize)
            .into_iter()
            .map(|m| {
                let label = &self.candidates[m.index].label;
                let positions = m.positions.iter().map(|&byte| label[..byte].encode_utf16().count() as u32).collect();
                PaletteMatch { index: m.index as u32, score: m.score, positions }
            })
            .collect()
    }
}
