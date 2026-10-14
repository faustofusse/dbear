//! The command palette (⌘K / Ctrl+K, everywhere in the window): the menus' commands, the open
//! tabs, the connections and the tables of every connection listed so far, in one search.
//! Matching and ranking are the core's (`dbcore::palette`), shared with the macOS app.
//!
//! Tab on a connection (or one of its databases) connects to it and keeps the palette open on its
//! tables, shown as `[app_dev ›] search…` in the field. Backspace in the empty field, or Escape once
//! it's empty, goes back to everything.
//!
//! The search field is the palette's own (in the `Command`'s header) rather than the `Command`'s,
//! so the entered connection can sit in it.

use std::ops::Range;

use dbcore::TableInfo;
use dbcore::palette::{self as search, Candidate};
use crate::workspace::{PaletteTables, Workspace};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

actions!(dbear, [OpenPalette, EnterConnection, Back, Dismiss, StayInField]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::base::actions::{SelectDown, SelectUp};
    cx.bind_keys([
        // No context: it opens from the SQL editor and the grid too.
        KeyBinding::new("secondary-k", OpenPalette, None),
        // ⌃N / ⌃P move through the list too, like ↓ / ↑ (over the search field's own bindings).
        KeyBinding::new("ctrl-n", SelectDown, Some("Command > Input")),
        KeyBinding::new("ctrl-p", SelectUp, Some("Command > Input")),
        // Tab never moves focus out of the field (the window's Tab would).
        KeyBinding::new("tab", EnterConnection, Some("Command > Input")),
        KeyBinding::new("shift-tab", StayInField, Some("Command > Input")),
        // Only in the empty field; otherwise Backspace deletes as usual.
        KeyBinding::new("backspace", Back, Some("Command > Input")),
        KeyBinding::new("escape", Dismiss, Some("Command > Input")),
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

/// Entries as searched and as shown.
#[derive(Default)]
struct List {
    candidates: Vec<Candidate>,
    entries: Vec<Shown>,
}

impl List {
    fn new(entries: Vec<Entry>) -> Self {
        let (candidates, entries) = entries
            .into_iter()
            .map(|e| {
                let label = SharedString::from(e.candidate.label.clone());
                (e.candidate, Shown { label, detail: e.detail, icon: e.icon, shortcut: e.shortcut, target: e.target })
            })
            .unzip();
        Self { candidates, entries }
    }
}

/// The connection (and database) the palette was narrowed to with Tab: only its tables.
struct Scope {
    key: (String, String),
    name: SharedString,
    icon: Icon,
    tables: PaletteTables,
    list: List,
}

pub struct Palette {
    state: Entity<CommandState>,
    input: Entity<InputState>,
    workspace: WeakEntity<Workspace>,
    all: List,
    scope: Option<Scope>,
    matches: Vec<search::Match>,
    /// What had focus before, where commands are sent (and focus goes back to).
    return_focus: Option<FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

const PLACEHOLDER: &str = "Search commands, tabs, connections and tables";

impl Palette {
    /// Opens the palette over the window, listing `entries`.
    pub fn open(entries: Vec<Entry>, workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let return_focus = window.focused(cx);
        let palette = cx.new(|cx| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
            let subscriptions = vec![
                cx.subscribe_in(&input, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.search(window, cx);
                    }
                }),
                // A connection entered with Tab lists its tables once they're in.
                cx.observe(&workspace, |this: &mut Self, _, cx| this.refresh_scope(cx)),
            ];
            let mut palette = Self {
                state: cx.new(|cx| CommandState::new(window, cx)),
                input,
                workspace: workspace.downgrade(),
                all: List::new(entries),
                scope: None,
                matches: Vec::new(),
                return_focus,
                _subscriptions: subscriptions,
            };
            palette.rank(cx);
            palette
        });
        let content = palette.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(600.)).margin_top(px(90.)).p_0().close_button(false).child(content.clone())
        });
        let focus = palette.read(cx).input.focus_handle(cx);
        window.focus(&focus, cx);
        palette
    }

    fn list(&self) -> &List {
        self.scope.as_ref().map_or(&self.all, |s| &s.list)
    }

    fn query(&self, cx: &App) -> SharedString {
        self.input.read(cx).value()
    }

    /// The field changed: rank again, with the first row selected.
    fn search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rank(cx);
        self.select_first(window, cx);
    }

    fn rank(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        self.matches = match &self.scope {
            // Its tables only, all of them before anything's typed.
            Some(scope) => search::rank(&format!("#{}", query.trim_start_matches(['#', '>', '@'])), &scope.list.candidates, LIMIT),
            None => search::rank(&query, &self.all.candidates, LIMIT),
        };
        cx.notify();
    }

    fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.set_value(query.to_string(), window, cx));
        self.rank(cx);
        self.select_first(window, cx);
    }

    /// The rows are installed on the next render, which keeps the selected row's index: select the
    /// first one now, while the old rows are still there.
    fn select_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_index().is_some() {
            self.state.update(cx, |state, cx| state.set_selected_index(Some(IndexPath::new(0)), window, cx));
        }
    }

    /// Tab: connects to the selected connection or database and lists its tables.
    fn enter_connection(&mut self, _: &EnterConnection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.state.read(cx).selected_index().map(|ix| ix.row) else { return };
        let Some(Target::Connection { id, database }) = self.target(row) else { return };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let Some((key, name, icon)) = workspace.update(cx, |w, cx| w.palette_enter(id, database, cx)) else { return };
        self.scope = Some(Scope { key, name: name.into(), icon, tables: PaletteTables::Loading, list: List::default() });
        self.input.update(cx, |input, cx| input.set_placeholder("Search its tables", window, cx));
        self.refresh_scope(cx);
        self.set_query("", window, cx);
        let focus = self.input.focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn leave_scope(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scope = None;
        // Connecting may have made more commands and tables available.
        if let (Some(workspace), Some(focus)) = (self.workspace.upgrade(), self.return_focus.clone()) {
            self.all = List::new(workspace.read(cx).palette_entries(&focus, window, cx));
        }
        self.input.update(cx, |input, cx| input.set_placeholder(PLACEHOLDER, window, cx));
        self.set_query("", window, cx);
    }

    /// Backspace: in the empty field, leaves the entered connection.
    fn back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope.is_some() && self.query(cx).is_empty() {
            self.leave_scope(window, cx);
        } else {
            cx.propagate();
        }
    }

    /// Escape: clears the field, then leaves the entered connection, then closes.
    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query(cx).is_empty() {
            self.set_query("", window, cx);
        } else if self.scope.is_some() {
            self.leave_scope(window, cx);
        } else {
            window.close_dialog(cx);
        }
    }

    /// Picks up the entered connection's tables (or why they couldn't be listed).
    fn refresh_scope(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = &self.scope else { return };
        if !matches!(scope.tables, PaletteTables::Loading) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        let (tables, entries) = workspace.read(cx).palette_tables(&scope.key, cx);
        if let Some(scope) = &mut self.scope {
            scope.tables = tables;
            scope.list = List::new(entries);
        }
        self.rank(cx);
    }

    /// What the row at `row` does.
    fn target(&self, row: usize) -> Option<Target> {
        let m = self.matches.get(row)?;
        Some(self.list().entries[m.index].target.clone())
    }

    fn confirm(this: &Entity<Self>, row: usize, window: &mut Window, cx: &mut App) {
        let (target, return_focus, workspace) = {
            let palette = this.read(cx);
            (palette.target(row), palette.return_focus.clone(), palette.workspace.clone())
        };
        let Some(target) = target else { return };
        window.close_dialog(cx);
        match target {
            // Like the menus: handled by whatever has focus (the grid, the editor, the window).
            Target::Action(action) => match return_focus {
                Some(focus) => focus.dispatch_action(action.as_ref(), window, cx),
                None => window.dispatch_action(action, cx),
            },
            target => {
                workspace.update(cx, |w, cx| w.palette_chose(target, window, cx)).ok();
            }
        }
    }

    /// What the field and the empty list show about the entered connection.
    fn scope_view(&self) -> Option<ScopeView> {
        let scope = self.scope.as_ref()?;
        Some(ScopeView { name: scope.name.clone(), icon: scope.icon.clone(), tables: scope.tables.clone() })
    }

    fn item(&self, m: &search::Match) -> CommandItem {
        let entry = &self.list().entries[m.index];
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

#[derive(Clone)]
struct ScopeView {
    name: SharedString,
    icon: Icon,
    tables: PaletteTables,
}

/// The search field: the entered connection (if any) before what's typed, and a spinner while its
/// tables load.
fn field(input: &Entity<InputState>, scope: Option<&ScopeView>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let loading = scope.is_some_and(|s| matches!(s.tables, PaletteTables::Loading));
    h_flex()
        .w_full()
        .px_3()
        .py_1()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(Icon::new(IconName::Search).text_color(theme.muted_foreground))
        .children(scope.map(|scope| {
            h_flex()
                .flex_shrink_0()
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .bg(theme.secondary)
                .text_sm()
                .text_color(theme.secondary_foreground)
                .child(scope.icon.clone().xsmall())
                .child(scope.name.clone())
                .child(Icon::new(IconName::ChevronRight).xsmall().text_color(theme.muted_foreground))
        }))
        .child(div().flex_1().child(Input::new(input).appearance(false).p_0()))
        .when(loading, |row| row.child(Spinner::new().small().color(theme.muted_foreground)))
        .into_any_element()
}

/// Instead of the rows when there are none: loading the entered connection's tables, why they
/// couldn't be listed, or no matches.
fn status(scope: Option<&ScopeView>, query_empty: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let row = h_flex().py_6().px_4().w_full().justify_center().gap_2().text_sm().text_color(theme.muted_foreground);
    match scope.map(|s| &s.tables) {
        Some(PaletteTables::Loading) => row.child(Spinner::new().small()).child("Loading tables\u{2026}").into_any_element(),
        Some(PaletteTables::Failed(e)) => row.text_color(theme.danger).child(e.clone()).into_any_element(),
        Some(PaletteTables::Ready) if query_empty => row.child("No tables").into_any_element(),
        _ => row.child("No matches").into_any_element(),
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let items: Vec<CommandItem> = self.matches.iter().map(|m| self.item(m)).collect();
        let (input, scope) = (self.input.clone(), self.scope_view());
        let (empty_scope, query_empty) = (scope.clone(), self.query(cx).is_empty());
        let scoped = self.scope.as_ref().map(|s| s.list.entries.len());
        div()
            .w_full()
            // ⌘K again closes it.
            .on_action(|_: &OpenPalette, window, cx| window.close_dialog(cx))
            .on_action(cx.listener(Self::enter_connection))
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::dismiss))
            .on_action(|_: &StayInField, _, _| {})
            .child(
                Command::new(&self.state)
                    .searchable(false)
                    .bordered(false)
                    .max_h(px(420.))
                    .items(items)
                    .header(move |_, _, cx| field(&input, scope.as_ref(), cx))
                    .on_confirm(move |ix, window, cx| Self::confirm(&this, ix.row, window, cx))
                    .empty(move |_, _, cx| status(empty_scope.as_ref(), query_empty, cx))
                    .footer(move |_, _, cx| {
                        let theme = cx.theme();
                        h_flex()
                            .px_3()
                            .py_1p5()
                            .gap_3()
                            .border_t_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .map(|footer| match scoped {
                                None => footer
                                    .child("> commands")
                                    .child("# tables")
                                    .child("@ connections")
                                    .child(div().flex_1())
                                    .child("\u{21e5} a connection\u{2019}s tables"),
                                Some(count) => footer
                                    .child(format!("{count} tables"))
                                    .child(div().flex_1())
                                    .child("\u{21b5} open")
                                    .child("\u{232b} everything"),
                            })
                    }),
            )
    }
}
