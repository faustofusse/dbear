//! The value inspector: the selected cell in full (long text, pretty-printed JSON), beside the grid.
//! Shown or hidden for every tab at once (⌥⌘I), like the macOS app.

use dbcore::Value;
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::grid::{RowsDelegate, copy};

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

pub struct Inspector {
    grid: Entity<TableState<RowsDelegate>>,
    /// The cell shown: (row, column).
    focus: Option<(usize, usize)>,
    /// Show text as stored instead of pretty-printed JSON.
    raw: bool,
    _subscriptions: [Subscription; 2],
}

impl Inspector {
    pub fn new(grid: Entity<TableState<RowsDelegate>>, cx: &mut Context<Self>) -> Self {
        let subscriptions = [
            cx.subscribe(&grid, |this, _, event: &TableEvent, cx| {
                match *event {
                    TableEvent::SelectCell(row, col) => this.focus = Some((row, col)),
                    // A whole row: keep the column that was being looked at.
                    TableEvent::SelectRow(row) => this.focus = Some((row, this.focus.map_or(0, |(_, col)| col))),
                    _ => return,
                }
                cx.notify();
            }),
            // New rows (a reload or a new result) replace what was focused.
            cx.observe(&grid, |this, grid, cx| {
                let grid = grid.read(cx).delegate();
                if let Some((row, col)) = this.focus {
                    if row >= grid.rows.len() || col >= grid.columns.len() {
                        this.focus = None;
                    }
                }
                cx.notify();
            }),
        ];
        Self { grid, focus: None, raw: false, _subscriptions: subscriptions }
    }
}

impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let panel = v_flex().size_full().border_l_1().border_color(theme.border).bg(theme.sidebar);
        let grid = self.grid.read(cx).delegate();
        let Some((row, col)) = self.focus.filter(|&(r, c)| r < grid.rows.len() && c < grid.columns.len()) else {
            return panel
                .items_center()
                .justify_center()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Select a cell to inspect it");
        };
        let column = &grid.columns[col];
        let value = grid.rows[row][col].clone();
        let text = if value.is_null() { String::new() } else { value.display() };
        let pretty = match &value {
            Value::Text(t) => dbcore::export::pretty_json(t),
            _ => None,
        };
        let shown = match (&pretty, self.raw) {
            (Some(pretty), false) => pretty.clone(),
            _ => text.clone(),
        };
        let (name, type_name) = (column.name.clone(), column.type_name.clone());

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
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.raw = !this.raw;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(Button::new("copy-value").ghost().xsmall().icon(IconName::Copy).tooltip("Copy Value").on_click(
                        move |_, _, cx| copy(text.clone(), cx),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{type_name} · row {}", row + 1)),
            );

        let body = match &value {
            Value::Null => div().italic().text_color(theme.muted_foreground).child("NULL").into_any_element(),
            _ if shown.is_empty() => div().italic().text_color(theme.muted_foreground).child("Empty").into_any_element(),
            _ => SelectableText::new(("inspected", row * 10_000 + col), shown).into_any_element(),
        };
        panel.child(header).child(
            div()
                .id("inspector-body")
                .flex_1()
                .min_h_0()
                .p_3()
                .font_family(theme.mono_font_family.clone())
                .text_sm()
                .overflow_y_scrollbar()
                .child(body),
        )
    }
}

use gpui_kit::prelude::FluentBuilder as _;
