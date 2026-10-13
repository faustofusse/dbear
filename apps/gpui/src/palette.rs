//! The command palette (⌘K / Ctrl+K, everywhere in the window): the menus' commands, the open
//! tabs, the connections and the tables of every connection listed so far, in one search.
//! Matching and ranking are the core's (`dbcore::palette`), shared with the macOS app.

use std::ops::Range;
use std::rc::Rc;

use dbcore::TableInfo;
use dbcore::palette::{self as search, Candidate};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::*;

actions!(dbear, [OpenPalette]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::base::actions::{SelectDown, SelectUp};
    cx.bind_keys([
        // No context: it opens from the SQL editor and the grid too.
        KeyBinding::new("secondary-k", OpenPalette, None),
        // ⌃N / ⌃P move through the list too, like ↓ / ↑ (over the search field's own bindings).
        KeyBinding::new("ctrl-n", SelectDown, Some("Command > Input")),
        KeyBinding::new("ctrl-p", SelectUp, Some("Command > Input")),
    ]);
}

/// Rows shown at most: the best matches are on top, and a short list keeps typing fast.
const LIMIT: usize = 60;

/// What picking an entry does.
pub enum Target {
    /// A menu command, sent to what had focus before the palette opened.
    Action(Box<dyn Action>),
    /// Switch to the open tab showing this view.
    Tab(EntityId),
    /// Select a connection (connecting to it), or one of its databases.
    Connection { id: String, database: Option<String> },
    /// Open a table of a connection and database already listed.
    Table { connection: String, database: String, table: TableInfo },
}

impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Target::Action(action) => Target::Action(action.boxed_clone()),
            Target::Tab(id) => Target::Tab(*id),
            Target::Connection { id, database } => Target::Connection { id: id.clone(), database: database.clone() },
            Target::Table { connection, database, table } => {
                Target::Table { connection: connection.clone(), database: database.clone(), table: table.clone() }
            }
        }
    }
}

/// Handles what the palette doesn't itself (everything but commands).
type Choose = Rc<dyn Fn(Target, &mut Window, &mut App)>;

/// One row of the palette.
pub struct Entry {
    pub candidate: Candidate,
    /// Muted, after the label: a table's schema and connection, a tab's connection…
    pub detail: Option<SharedString>,
    pub icon: Icon,
    pub shortcut: Option<Keystroke>,
    pub target: Target,
}

struct Shown {
    label: SharedString,
    detail: Option<SharedString>,
    icon: Icon,
    shortcut: Option<Keystroke>,
    target: Target,
}

pub struct Palette {
    state: Entity<CommandState>,
    candidates: Vec<Candidate>,
    entries: Vec<Shown>,
    matches: Vec<search::Match>,
    /// What had focus before, where commands are sent (and focus goes back to).
    return_focus: Option<FocusHandle>,
    choose: Choose,
}

impl Palette {
    /// Opens the palette over the window. `choose` handles tabs, connections and tables.
    pub fn open(
        entries: Vec<Entry>,
        choose: impl Fn(Target, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let return_focus = window.focused(cx);
        let palette = cx.new(|cx| Self::new(entries, return_focus, Rc::new(choose), window, cx));
        let content = palette.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(600.)).margin_top(px(90.)).p_0().close_button(false).child(content.clone())
        });
        let focus = palette.read(cx).state.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        palette
    }

    fn new(
        entries: Vec<Entry>,
        return_focus: Option<FocusHandle>,
        choose: Choose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        let (candidates, entries): (Vec<Candidate>, Vec<Shown>) = entries
            .into_iter()
            .map(|e| {
                let label = SharedString::from(e.candidate.label.clone());
                (e.candidate, Shown { label, detail: e.detail, icon: e.icon, shortcut: e.shortcut, target: e.target })
            })
            .unzip();
        let matches = search::rank("", &candidates, LIMIT);
        Self { state, candidates, entries, matches, return_focus, choose }
    }

    fn search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.matches = search::rank(query, &self.candidates, LIMIT);
        cx.notify();
    }

    /// What the row at `row` does.
    fn target(&self, row: usize) -> Option<Target> {
        let m = self.matches.get(row)?;
        Some(self.entries[m.index].target.clone())
    }

    fn confirm(this: &Entity<Self>, row: usize, window: &mut Window, cx: &mut App) {
        let (target, return_focus, choose) = {
            let palette = this.read(cx);
            (palette.target(row), palette.return_focus.clone(), palette.choose.clone())
        };
        let Some(target) = target else { return };
        window.close_dialog(cx);
        match target {
            // Like the menus: handled by whatever has focus (the grid, the editor, the window).
            Target::Action(action) => match return_focus {
                Some(focus) => focus.dispatch_action(action.as_ref(), window, cx),
                None => window.dispatch_action(action, cx),
            },
            target => choose(target, window, cx),
        }
    }

    fn item(&self, m: &search::Match) -> CommandItem {
        let entry = &self.entries[m.index];
        let label = entry.label.clone();
        let highlights: Vec<Range<usize>> = m
            .positions
            .iter()
            .map(|&at| at..at + label[at..].chars().next().map_or(1, char::len_utf8))
            .collect();
        let (detail, icon, shortcut) = (entry.detail.clone(), entry.icon.clone(), entry.shortcut.clone());
        CommandItem::new().label(label.clone()).child(move |_, cx| {
            let theme = cx.theme();
            let bold = HighlightStyle { font_weight: Some(FontWeight::BOLD), color: Some(theme.foreground), ..Default::default() };
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .child(icon.clone().small().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w(relative(0.7))
                        .truncate()
                        .child(StyledText::new(label.clone()).with_highlights(highlights.iter().map(|r| (r.clone(), bold)))),
                )
                .children(detail.clone().map(|d| div().min_w_0().flex_1().truncate().text_xs().text_color(theme.muted_foreground).child(d)))
                .child(div().flex_1())
                .children(shortcut.clone().map(|k| Kbd::new(k).flex_shrink_0()))
        })
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let weak = this.downgrade();
        let items: Vec<CommandItem> = self.matches.iter().map(|m| self.item(m)).collect();
        div()
            .w_full()
            // ⌘K again closes it.
            .on_action(|_: &OpenPalette, window, cx| window.close_dialog(cx))
            .child(
                Command::new(&self.state)
                    .filterable(false)
                    .bordered(false)
                    .max_h(px(420.))
                    .placeholder("Search commands, tabs, connections and tables")
                    .items(items)
                    .on_query(move |query, _, cx| {
                        weak.update(cx, |palette, cx| palette.search(query, cx)).ok();
                    })
                    .on_confirm(move |ix, window, cx| Self::confirm(&this, ix.row, window, cx))
                    .empty(|_, _, cx| {
                        div().py_6().w_full().text_center().text_sm().text_color(cx.theme().muted_foreground).child("No matches")
                    })
                    .footer(|_, _, cx| {
                        h_flex()
                            .px_3()
                            .py_1p5()
                            .gap_3()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("> commands")
                            .child("# tables")
                            .child("@ connections")
                    }),
            )
    }
}
