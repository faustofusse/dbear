//! The command palette's search (⌘K / Ctrl+K in both apps): fuzzy matching and ranking.
//!
//! The frontends list what can be picked (commands, tables, connections, open tabs) as
//! [`Candidate`]s and show what [`rank`] returns, so both apps find and order things the same way.
//!
//! A query is one or more space-separated terms; each must match, in order of its characters but
//! not necessarily adjacent, the label or one of the keywords (`uord` finds `user_orders`).
//! Matches at the start of words, and runs of adjacent characters, rank higher. A leading `>` only
//! searches commands, `#` tables and `@` connections.

/// What a candidate is: decides the prefix that finds it and what an empty query lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Command,
    Table,
    Connection,
    Tab,
}

/// Something the palette can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: Kind,
    /// What's shown (and highlighted where it matched).
    pub label: String,
    /// More text that finds it but isn't highlighted: a table's schema and connection, a
    /// connection's host…
    pub keywords: Vec<String>,
}

impl Candidate {
    pub fn new(kind: Kind, label: impl Into<String>) -> Self {
        Self { kind, label: label.into(), keywords: Vec::new() }
    }

    pub fn keywords<S: Into<String>>(mut self, keywords: impl IntoIterator<Item = S>) -> Self {
        self.keywords.extend(keywords.into_iter().map(Into::into));
        self
    }
}

/// A candidate that matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// Index into the candidates passed to [`rank`].
    pub index: usize,
    pub score: i32,
    /// Byte offsets of the label's matched characters, ascending.
    pub positions: Vec<usize>,
}

/// The `>`, `#` and `@` prefixes, and the rest of the query.
pub fn scope(query: &str) -> (Option<Kind>, &str) {
    let query = query.trim_start();
    let kind = match query.chars().next() {
        Some('>') => Kind::Command,
        Some('#') => Kind::Table,
        Some('@') => Kind::Connection,
        _ => return (None, query),
    };
    (Some(kind), &query[1..])
}

/// The candidates that match `query`, best first (ties keep the order they were given in), at
/// most `limit`.
///
/// An empty query lists the candidates as given, except tables unless the query is `#`: there
/// can be thousands of them, and they're found by typing.
pub fn rank(query: &str, candidates: &[Candidate], limit: usize) -> Vec<Match> {
    let (scope, rest) = scope(query);
    let terms: Vec<Vec<char>> = rest.split_whitespace().map(|t| t.chars().collect()).collect();
    let in_scope = |c: &Candidate| scope.is_none_or(|k| c.kind == k);
    if terms.is_empty() {
        return candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| in_scope(c) && (scope.is_some() || c.kind != Kind::Table))
            .take(limit)
            .map(|(index, _)| Match { index, score: 0, positions: Vec::new() })
            .collect();
    }
    let mut matches: Vec<(Match, usize)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| in_scope(c))
        .filter_map(|(index, c)| {
            let (score, positions) = score_candidate(&terms, c)?;
            Some((Match { index, score, positions }, c.label.chars().count()))
        })
        .collect();
    // Best score, then the shorter label (`orders` before `orders_archive`), then the given order.
    matches.sort_by(|(a, a_len), (b, b_len)| b.score.cmp(&a.score).then(a_len.cmp(b_len)).then(a.index.cmp(&b.index)));
    matches.truncate(limit);
    matches.into_iter().map(|(m, _)| m).collect()
}

/// Scores a keyword match below the same match on the label, even one typed out in full (`zoom`
/// finds "Zoom In" before "Actual Size", whose keyword it is).
const KEYWORD_PENALTY: i32 = 24;

fn score_candidate(terms: &[Vec<char>], candidate: &Candidate) -> Option<(i32, Vec<usize>)> {
    // Most candidates don't match at all: find out before allocating anything.
    let found = |term: &[char]| {
        std::iter::once(&candidate.label).chain(&candidate.keywords).any(|text| is_subsequence(term, text))
    };
    if !terms.iter().all(|term| found(term)) {
        return None;
    }
    let label = Text::new(&candidate.label);
    let keywords: Vec<Text> = candidate.keywords.iter().map(|k| Text::new(k)).collect();
    let mut total = 0;
    let mut positions = Vec::new();
    for term in terms {
        let on_label = fuzzy(term, &label);
        let on_keyword = keywords.iter().filter_map(|k| fuzzy(term, k)).map(|(score, _)| score - KEYWORD_PENALTY).max();
        match (on_label, on_keyword) {
            (Some((score, _)), Some(keyword)) if keyword > score => total += keyword,
            (Some((score, chars)), _) => {
                total += score;
                positions.extend(chars.into_iter().map(|i| label.offsets[i]));
            }
            (None, Some(score)) => total += score,
            (None, None) => return None,
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Some((total, positions))
}

fn is_subsequence(pattern: &[char], text: &str) -> bool {
    let mut chars = text.chars().map(fold);
    pattern.iter().all(|&p| {
        let p = fold(p);
        chars.any(|c| c == p)
    })
}

/// Matches `pattern` against `text`, the way [`rank`] does each term: the score and which of the
/// text's characters (as char indices) matched, or `None`.
pub fn fuzzy_match(pattern: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let pattern: Vec<char> = pattern.chars().collect();
    let text = Text::new(text);
    let (score, chars) = fuzzy(&pattern, &text)?;
    Some((score, chars.into_iter().map(|i| text.offsets[i]).collect()))
}

/// A text to match against: its characters, lowercased, and where each starts.
struct Text {
    chars: Vec<char>,
    lower: Vec<char>,
    offsets: Vec<usize>,
}

impl Text {
    fn new(text: &str) -> Self {
        let (offsets, chars): (Vec<usize>, Vec<char>) = text.char_indices().unzip();
        let lower = chars.iter().map(|&c| fold(c)).collect();
        Self { chars, lower, offsets }
    }

    /// The bonus for matching character `i`: more at the start of a word.
    fn bonus(&self, i: usize) -> i32 {
        if i == 0 {
            return BOUNDARY + FIRST_CHAR;
        }
        let (prev, c) = (self.chars[i - 1], self.chars[i]);
        if !prev.is_alphanumeric() && c.is_alphanumeric() {
            BOUNDARY
        } else if (prev.is_lowercase() && c.is_uppercase()) || (prev.is_alphabetic() && c.is_numeric()) {
            // camelCase, table2
            BOUNDARY - 2
        } else {
            0
        }
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

const MATCH: i32 = 16;
const BOUNDARY: i32 = 10;
const FIRST_CHAR: i32 = 4;
const CONSECUTIVE: i32 = 6;
const GAP_START: i32 = 3;
const GAP_EXTEND: i32 = 1;
/// Characters skipped before the first match cost this much each, up to `MAX_LEADING` of them.
const LEADING: i32 = 1;
const MAX_LEADING: i32 = 8;
const NONE: i32 = i32::MIN / 4;

/// The best alignment of `pattern` in `text` (smith-waterman-like, as fzf does): every pattern
/// character matched in order, scored by [`Text::bonus`], adjacency and gaps.
fn fuzzy(pattern: &[char], text: &Text) -> Option<(i32, Vec<usize>)> {
    let (m, n) = (pattern.len(), text.lower.len());
    if m == 0 {
        return Some((0, Vec::new()));
    }
    if m > n {
        return None;
    }
    let pattern: Vec<char> = pattern.iter().map(|&c| fold(c)).collect();
    // Cheap rejection: is it a subsequence at all?
    let mut it = text.lower.iter();
    if !pattern.iter().all(|p| it.any(|c| c == p)) {
        return None;
    }

    // score[i][j]: the best score with pattern[i] matched at text[j]; from[i][j]: where pattern[i-1] was.
    let mut score = vec![NONE; m * n];
    let mut from = vec![usize::MAX; m * n];
    for (j, &c) in text.lower.iter().enumerate() {
        if c == pattern[0] {
            score[j] = MATCH + text.bonus(j) - LEADING * (j as i32).min(MAX_LEADING);
        }
    }
    for (i, &p) in pattern.iter().enumerate().skip(1) {
        let (prev, row) = ((i - 1) * n, i * n);
        // The best previous match at least two characters back, minus the gap to j.
        let (mut gap_best, mut gap_from) = (NONE, usize::MAX);
        for j in i..n {
            if j >= 2 && score[prev + j - 2] > NONE && score[prev + j - 2] - GAP_START > gap_best - GAP_EXTEND {
                gap_best = score[prev + j - 2] - GAP_START;
                gap_from = j - 2;
            } else {
                gap_best -= GAP_EXTEND;
            }
            if text.lower[j] != p {
                continue;
            }
            let adjacent = score[prev + j - 1];
            let (best, k) = if adjacent > NONE && adjacent + CONSECUTIVE >= gap_best {
                (adjacent + CONSECUTIVE, j - 1)
            } else {
                (gap_best, gap_from)
            };
            if best > NONE / 2 && k != usize::MAX {
                score[row + j] = best + MATCH + text.bonus(j);
                from[row + j] = k;
            }
        }
    }
    let last = (m - 1) * n;
    let (end, &best) = score[last..].iter().enumerate().max_by_key(|&(j, s)| (*s, std::cmp::Reverse(j)))?;
    if best <= NONE / 2 {
        return None;
    }
    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = j;
        j = from[i * n + j];
    }
    // The whole text, typed out.
    let exact = if m == n { MATCH } else { 0 };
    Some((best + exact, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(query: &str, candidates: &[Candidate]) -> Vec<String> {
        rank(query, candidates, 100).into_iter().map(|m| candidates[m.index].label.clone()).collect()
    }

    fn tables(names: &[&str]) -> Vec<Candidate> {
        names.iter().map(|n| Candidate::new(Kind::Table, *n)).collect()
    }

    fn highlighted(query: &str, label: &str) -> String {
        let m = rank(query, &[Candidate::new(Kind::Command, label)], 1).pop().expect("a match");
        label
            .char_indices()
            .map(|(i, c)| if m.positions.contains(&i) { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
            .collect()
    }

    #[test]
    fn matches_characters_in_order() {
        assert!(fuzzy_match("uord", "user_orders").is_some());
        assert!(fuzzy_match("UORD", "user_orders").is_some());
        assert!(fuzzy_match("drou", "user_orders").is_none());
        assert!(fuzzy_match("x", "user_orders").is_none());
        assert!(fuzzy_match("user_orders_", "user_orders").is_none());
    }

    #[test]
    fn prefers_word_starts() {
        assert_eq!(highlighted("uord", "user_orders"), "User_ORDers");
        assert_eq!(highlighted("ns", "New SQL Script"), "New Sql script");
        assert_eq!(highlighted("nsc", "New SQL Script"), "New sql SCript");
        assert_eq!(highlighted("ui", "userId"), "UserId");
        assert_eq!(highlighted("uid", "userId"), "UserID");
    }

    #[test]
    fn ranks_better_matches_first() {
        let candidates = tables(&["big_order_records", "user_orders", "orders_archive", "orders", "border"]);
        let found = labels("ord", &candidates);
        assert_eq!(found.len(), 5);
        assert_eq!(found[..2], ["orders", "orders_archive"]);
        assert_eq!(found.last().unwrap(), "border");
        assert_eq!(labels("orders", &candidates)[..3], ["orders", "orders_archive", "user_orders"]);
        assert_eq!(labels("uo", &candidates), ["user_orders"]);
    }

    #[test]
    fn every_term_must_match_the_label_or_a_keyword() {
        let candidates = vec![
            Candidate::new(Kind::Table, "orders").keywords(["public.orders", "app_dev"]),
            Candidate::new(Kind::Table, "orders").keywords(["sales.orders", "Production"]),
        ];
        let found = rank("ord prod", &candidates, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].index, 1);
        // Only the label is highlighted.
        assert_eq!(found[0].positions, [0, 1, 2]);
        assert_eq!(rank("sales.ord", &candidates, 10).len(), 1);
        assert!(rank("ord nope", &candidates, 10).is_empty());
    }

    #[test]
    fn a_label_match_beats_the_same_keyword_match() {
        let candidates =
            vec![Candidate::new(Kind::Connection, "Local").keywords(["users"]), Candidate::new(Kind::Table, "users")];
        assert_eq!(labels("users", &candidates), ["users", "Local"]);
    }

    #[test]
    fn a_whole_keyword_ranks_below_a_label_match() {
        let candidates = vec![
            Candidate::new(Kind::Command, "Actual Size").keywords(["zoom"]),
            Candidate::new(Kind::Command, "Zoom In").keywords(["font"]),
        ];
        assert_eq!(labels("zoom", &candidates), ["Zoom In", "Actual Size"]);
    }

    #[test]
    fn prefixes_limit_the_kind() {
        let candidates = vec![
            Candidate::new(Kind::Command, "Toggle Inspector"),
            Candidate::new(Kind::Table, "items"),
            Candidate::new(Kind::Connection, "Integration"),
            Candidate::new(Kind::Tab, "items"),
        ];
        assert_eq!(rank(">i", &candidates, 10).iter().map(|m| m.index).collect::<Vec<_>>(), [0]);
        assert_eq!(rank("#i", &candidates, 10).iter().map(|m| m.index).collect::<Vec<_>>(), [1]);
        assert_eq!(rank("@ i", &candidates, 10).iter().map(|m| m.index).collect::<Vec<_>>(), [2]);
        assert_eq!(rank("i", &candidates, 10).len(), 4);
    }

    #[test]
    fn an_empty_query_lists_all_but_tables() {
        let candidates = vec![
            Candidate::new(Kind::Tab, "SQL 1"),
            Candidate::new(Kind::Table, "items"),
            Candidate::new(Kind::Command, "New SQL Script"),
        ];
        assert_eq!(rank("  ", &candidates, 10).iter().map(|m| m.index).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(rank("#", &candidates, 10).iter().map(|m| m.index).collect::<Vec<_>>(), [1]);
        assert_eq!(rank("", &candidates, 1).len(), 1);
    }

    #[test]
    fn positions_are_byte_offsets() {
        let m = rank("ña", &[Candidate::new(Kind::Table, "año_ña")], 1).pop().unwrap();
        assert_eq!(m.positions, ["año_".len(), "año_ñ".len()]);
    }

    #[test]
    fn limits_the_results() {
        let candidates = tables(&["a1", "a2", "a3"]);
        assert_eq!(rank("a", &candidates, 2).len(), 2);
    }
}
