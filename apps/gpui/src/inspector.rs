//! The value inspector: the selected cell in full (long text, pretty-printed JSON), beside the grid.
//! Shown or hidden for every tab at once (⌥⌘I), like the macOS app. On an editable table it edits
//! the value too: Apply stages it like an edit in the grid (saved with the others, ⌘S).

use std::cell::Cell;
use std::rc::Rc;

use dbcore::edit::EditValue;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::grid::{RowsDelegate, copy};
use crate::highlight;

actions!(dbear, [ApplyInspected]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("secondary-enter", ApplyInspected, Some("Inspector > Input"))]);
}

/// Whether inspectors are shown (one switch for the whole app).
pub struct ShowInspector(pub bool);

impl Global for ShowInspector {}

pub fn is_shown(cx: &App) -> bool {
    cx.try_global::<ShowInspector>().is_some_and(|s| s.0)
}

pub fn toggle(cx: &mut App) {
    let shown = is_shown(cx);
    cx.set_global(ShowInspector(!shown));
    cx.refresh_windows();
}

/// Whether inspectors wrap long lines (one switch for the whole app, on by default).
pub struct WrapLines(pub bool);

impl Global for WrapLines {}

fn wraps_lines(cx: &App) -> bool {
    cx.try_global::<WrapLines>().is_none_or(|w| w.0)
}

/// The editor's horizontal scroll, for an always-visible scrollbar of our own (the editor's own
/// follows the system setting, so it only shows while scrolling). The editor doesn't expose its
/// scroll handle: this mirrors its offset each frame, and a drag is applied on the next render.
#[derive(Clone, Default)]
struct HorizontalScroll {
    offset: Rc<Cell<Point<Pixels>>>,
    dragged_to: Rc<Cell<Option<Point<Pixels>>>>,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    content: Rc<Cell<Size<Pixels>>>,
}

impl ScrollbarHandle for HorizontalScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.viewport.get()
    }

    fn offset(&self) -> Point<Pixels> {
        self.offset.get()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.offset.set(offset);
        self.dragged_to.set(Some(offset));
    }

    fn content_size(&self) -> Size<Pixels> {
        self.content.get()
    }
}

/// Space the editor leaves after the longest line (gpui-base's `RIGHT_MARGIN`).
const EDITOR_RIGHT_MARGIN: Pixels = px(10.);

pub struct Inspector {
    grid: Entity<TableState<RowsDelegate>>,
    /// The cell shown: (row, column).
    focus: Option<(usize, usize)>,
    /// Show text as stored instead of pretty-printed JSON.
    raw: bool,
    editor: Entity<EditorState>,
    /// What the editor was loaded with: Apply is enabled once the text differs.
    loaded: String,
    horizontal: HorizontalScroll,
    /// Width of the longest line, measured for the text of this length (re-measured when it changes).
    longest_line: Option<(usize, Pixels)>,
    _subscriptions: Vec<Subscription>,
}

impl Inspector {
    pub fn new(grid: Entity<TableState<RowsDelegate>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            // No gutter at all: no line numbers, no fold markers. No empty space after the last
            // line either, so a value that fits doesn't get a scrollbar.
            let mut state = EditorState::new(window, cx)
                .soft_wrap(wraps_lines(cx))
                .line_number(false)
                .folding(false)
                .scroll_beyond_last_line(Some(0))
                .language(highlight::PLAIN);
            state.set_highlighter_factory(highlight::factory(), cx);
            state
        });
        let subscriptions = vec![
            cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| {
                let focus = match *event {
                    TableEvent::SelectCell(row, col) => (row, col),
                    // A whole row: keep the column that was being looked at.
                    TableEvent::SelectRow(row) => (row, this.focus.map_or(0, |(_, col)| col)),
                    _ => return,
                };
                this.focus = Some(focus);
                this.load(window, cx);
            }),
            // New rows (a reload or a new result) can replace what was focused.
            cx.observe(&grid, |this, grid, cx| {
                let grid = grid.read(cx).delegate();
                let rows = grid.rows.len() + grid.edits.inserted.len();
                if this.focus.is_some_and(|(row, col)| row >= rows || col >= grid.columns.len()) {
                    this.focus = None;
                }
                cx.notify();
            }),
            cx.observe(&editor, |_, _, cx| cx.notify()),
            // Toggled in another tab's inspector: follow it.
            cx.observe_global_in::<WrapLines>(window, |this, window, cx| {
                let wrap = wraps_lines(cx);
                this.editor.update(cx, |e, cx| e.set_soft_wrap(wrap, window, cx));
                cx.notify();
            }),
        ];
        Self {
            grid,
            focus: None,
            raw: false,
            editor,
            loaded: String::new(),
            horizontal: HorizontalScroll::default(),
            longest_line: None,
            _subscriptions: subscriptions,
        }
    }

    /// The focused cell's value as the grid shows it (with pending edits).
    fn shown(&self, cx: &App) -> Option<EditValue> {
        let (row, col) = self.focus?;
        self.grid.read(cx).delegate().shown(row, col)
    }

    fn pretty(value: &Option<EditValue>) -> Option<String> {
        match value {
            Some(EditValue::Text(text)) => dbcore::export::pretty_json(text),
            _ => None,
        }
    }

    /// Puts the focused value in the editor (pretty-printed JSON unless Raw).
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown(cx);
        let pretty = Self::pretty(&shown);
        // JSON is highlighted pretty or raw; anything else is plain text.
        let language = if pretty.is_some() { highlight::JSON } else { highlight::PLAIN };
        let text = match (&shown, pretty, self.raw) {
            (_, Some(pretty), false) => pretty,
            (Some(EditValue::Text(text)), _, _) => text.clone(),
            _ => String::new(),
        };
        let placeholder = match shown {
            Some(EditValue::Null) => "NULL",
            Some(EditValue::Default) => "DEFAULT",
            _ => "Empty",
        };
        self.loaded = text.clone();
        let readonly = !self.editable(cx);
        self.editor.update(cx, |e, cx| {
            e.set_readonly(readonly, cx);
            if e.language_name().as_ref() != language {
                e.set_highlighter(language, cx);
            }
            e.set_value(text, window, cx);
            e.set_placeholder(placeholder, window, cx);
        });
        cx.notify();
    }

    /// Our horizontal scrollbar for the editor when lines don't wrap; `None` while wrapping.
    fn horizontal_scrollbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if wraps_lines(cx) {
            return None;
        }
        if let Some(offset) = self.horizontal.dragged_to.take() {
            self.editor.update(cx, |e, cx| e.set_scroll_offset(offset, cx));
        } else {
            self.horizontal.offset.set(self.editor.read(cx).scroll_offset());
        }
        let editor = self.editor.read(cx);
        let viewport = editor.input_bounds();
        let text = editor.value();
        let width = match self.longest_line {
            Some((len, width)) if len == text.len() => width,
            _ => {
                // Monospaced: the line with the most characters is the widest.
                let longest = text.lines().max_by_key(|l| l.chars().count()).unwrap_or("").to_string();
                let font_size = window.rem_size() * 0.875; // the editor's `text_sm`
                let run = TextRun {
                    len: longest.len(),
                    font: font(cx.theme().mono_font_family.clone()),
                    color: Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let width = window.text_system().shape_line(longest.into(), font_size, &[run], None).width;
                self.longest_line = Some((text.len(), width));
                width
            }
        };
        self.horizontal.viewport.set(viewport);
        self.horizontal.content.set(size(width + EDITOR_RIGHT_MARGIN, viewport.size.height));
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(Scrollbar::width())
                .child(Scrollbar::horizontal(&self.horizontal).viewport_bounds(viewport).mode(ScrollbarMode::Always))
                .into_any_element(),
        )
    }

    fn editable(&self, cx: &App) -> bool {
        let Some((row, col)) = self.focus else { return false };
        let grid = self.grid.read(cx).delegate();
        grid.is_editable(col) && !grid.is_deleted(row)
    }

    fn apply(&mut self, _: &ApplyInspected, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.focus.filter(|_| self.editable(cx)) else { return };
        let text = self.editor.read(cx).value().to_string();
        if text == self.loaded {
            return;
        }
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().set_cell(row, col, EditValue::Text(text));
            cx.notify();
        });
        self.load(window, cx);
    }

    fn set(&mut self, value: EditValue, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.focus.filter(|_| self.editable(cx)) else { return };
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().set_cell(row, col, value);
            cx.notify();
        });
        self.load(window, cx);
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.focus else { return };
        self.grid.update(cx, |state, cx| {
            state.delegate_mut().revert_cell(row, col);
            cx.notify();
        });
        self.load(window, cx);
    }
}

impl Render for Inspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let horizontal_scrollbar = self.horizontal_scrollbar(window, cx);
        let theme = cx.theme();
        let panel = v_flex().size_full().border_l_1().border_color(theme.border).bg(theme.sidebar);
        let grid = self.grid.read(cx).delegate();
        let rows = grid.rows.len() + grid.edits.inserted.len();
        let Some((row, col)) = self.focus.filter(|&(r, c)| r < rows && c < grid.columns.len()) else {
            return panel
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Select a cell to inspect it");
        };
        let column = &grid.columns[col];
        let (name, type_name) = (column.name.clone(), column.type_name.clone());
        let shown = grid.shown(row, col);
        let edited = row < grid.rows.len() && grid.edits.updates.get(&row).is_some_and(|c| c.contains_key(&col));
        let pretty = Self::pretty(&shown);
        let editable = self.editable(cx);
        let copy_text = match &shown {
            Some(EditValue::Text(text)) => text.clone(),
            _ => String::new(),
        };

        let header = v_flex()
            .p_3()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().truncate().font_semibold().child(name))
                    .when(pretty.is_some(), |h| {
                        h.child(
                            Button::new("pretty")
                                .ghost()
                                .xsmall()
                                .label(if self.raw { "Pretty" } else { "Raw" })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.raw = !this.raw;
                                    this.load(window, cx);
                                })),
                        )
                    })
                    .child({
                        let wrap = wraps_lines(cx);
                        Button::new("wrap-lines")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(AssetIcon::TextWrap))
                            .selected(wrap)
                            .toggled(wrap)
                            .tooltip(if wrap { "Don't Wrap Lines" } else { "Wrap Lines" })
                            .on_click(move |_, _, cx| cx.set_global(WrapLines(!wrap)))
                    })
                    .child(
                        Button::new("copy-value")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy Value")
                            .on_click(move |_, _, cx| copy(copy_text.clone(), cx)),
                    ),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                "{type_name} · row {}{}",
                row + 1,
                if edited { " · edited" } else { "" }
            )));

        let dirty = editable && self.editor.read(cx).value().as_ref() != self.loaded;
        let footer = editable.then(|| {
            h_flex()
                .p_2()
                .gap_1()
                .border_t_1()
                .border_color(theme.border)
                .child(Button::new("set-null").ghost().xsmall().label("Set to NULL").on_click(cx.listener(
                    |this, _, window, cx| this.set(EditValue::Null, window, cx),
                )))
                .when(edited, |f| {
                    f.child(
                        Button::new("revert")
                            .ghost()
                            .xsmall()
                            .label("Revert")
                            .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx))),
                    )
                })
                .child(div().flex_1())
                .child(
                    Button::new("apply")
                        .primary()
                        .xsmall()
                        .label(format!("Apply ({})", crate::keys::shortcut("secondary-enter")))
                        .disabled(!dirty)
                        .on_click(cx.listener(|this, _, window, cx| this.apply(&ApplyInspected, window, cx))),
                )
        });
        // Read-only cells use the same editor (read-only), so text is selectable and JSON highlighted.
        panel
            .key_context("Inspector")
            .on_action(cx.listener(Self::apply))
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .py_1()
                    .child(
                        Editor::new(&self.editor)
                            .size_full()
                            .border_0()
                            .font_family(theme.mono_font_family.clone())
                            .text_sm(),
                    )
                    .children(horizontal_scrollbar),
            )
            .children(footer)
    }
}
