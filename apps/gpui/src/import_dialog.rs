//! "Import from DBeaver": lists DBeaver's saved connections to pick from, like the macOS sheet.
//! Finding and decrypting them is `dbcore::import::dbeaver`; this is only the list.

use std::collections::HashSet;
use std::path::PathBuf;

use dbcore::ConnectionConfig;
use dbcore::import::{ImportScan, ImportedConnection, dbeaver};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

enum State {
    Loading,
    Failed(String),
    Loaded(ImportScan),
}

pub struct ImportDialog {
    state: State,
    /// Indexes into the scan's connections.
    selected: HashSet<usize>,
    /// The file or folder chosen by hand (`None`: DBeaver's usual folders).
    source: Option<PathBuf>,
    /// Saved connections, to flag the ones already added.
    existing: Vec<ConnectionConfig>,
    error: Option<String>,
    _scan: Option<Task<()>>,
}

impl ImportDialog {
    pub fn new(existing: Vec<ConnectionConfig>, cx: &mut Context<Self>) -> Self {
        let mut dialog =
            Self { state: State::Loading, selected: HashSet::new(), source: None, existing, error: None, _scan: None };
        dialog.load(None, cx);
        dialog
    }

    fn load(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.state = State::Loading;
        self.source = path.clone();
        self.error = None;
        let existing = self.existing.clone();
        self._scan = Some(cx.spawn(async move |this, cx| {
            // Reads files and decrypts DBeaver's credentials: off the UI thread.
            let result = cx
                .background_executor()
                .spawn(async move {
                    let scan = match &path {
                        Some(path) => dbeaver::scan_path(path),
                        None => dbeaver::scan_default(),
                    };
                    scan.map(|scan| scan.mark_existing(&existing))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(scan) => {
                        // New ones start checked; the ones already added don't.
                        this.selected = (0..scan.connections.len()).filter(|&i| !scan.connections[i].already_added).collect();
                        this.state = State::Loaded(scan);
                    }
                    Err(e) => this.state = State::Failed(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Another `data-sources.json`, or a folder with them (a workspace, a project, DBeaverData…).
    pub fn choose_source(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            this.update(cx, |this, cx| this.load(Some(path), cx)).ok();
        })
        .detach();
    }

    fn connections(&self) -> &[ImportedConnection] {
        match &self.state {
            State::Loaded(scan) => &scan.connections,
            _ => &[],
        }
    }

    pub fn has_connections(&self) -> bool {
        !self.connections().is_empty()
    }

    pub fn all_selected(&self) -> bool {
        self.selected.len() == self.connections().len()
    }

    pub fn toggle_all(&mut self, cx: &mut Context<Self>) {
        self.selected = if self.all_selected() { HashSet::new() } else { (0..self.connections().len()).collect() };
        cx.notify();
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// The checked connections, in the listed order, with their imported passwords.
    pub fn selected_configs(&self) -> Vec<ConnectionConfig> {
        let connections = self.connections();
        let mut indexes: Vec<usize> = self.selected.iter().copied().collect();
        indexes.sort_unstable();
        indexes.into_iter().map(|i| connections[i].config.clone()).collect()
    }

    pub fn show_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        cx.notify();
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        cx.notify();
    }

    fn subtitle(&self) -> String {
        let source = self.source.as_ref().map(|p| p.display().to_string());
        let State::Loaded(scan) = &self.state else {
            return source.unwrap_or_else(|| "DBeaver’s saved connections".into());
        };
        let found = scan.connections.len();
        let added = scan.connections.iter().filter(|c| c.already_added).count();
        let mut text = format!("{found} connection{}", if found == 1 { "" } else { "s" });
        if added > 0 {
            text.push_str(&format!(", {added} already added"));
        }
        match source {
            Some(source) => format!("{text} · {source}"),
            None => text,
        }
    }

    fn render_row(&self, index: usize, item: &ImportedConnection, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let checked = self.selected.contains(&index);
        let this = cx.entity().downgrade();
        h_flex()
            .id(("import-row", index))
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded_md()
            .items_start()
            .hover(|s| s.bg(theme.muted))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(index, cx)))
            .child(div().pt_0p5().child(Checkbox::new(("import-check", index)).checked(checked).on_click(
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.toggle(index, cx)).ok();
                },
            )))
            .child(div().pt_0p5().child(crate::assets::kind_icon(item.config.kind).small().text_color(theme.muted_foreground)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().truncate().child(item.config.name.clone()))
                            .when(item.already_added, |h| {
                                h.child(div().text_xs().text_color(theme.muted_foreground).child("Already added"))
                            }),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(item.config.summary()))
                    .children(item.warnings.iter().map(|w| div().text_xs().text_color(theme.warning).child(w.clone()))),
            )
    }
}

impl Render for ImportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (muted, border, red) = (cx.theme().muted_foreground, cx.theme().border, cx.theme().red);
        let body = match &self.state {
            State::Loading => h_flex().size_full().justify_center().child(Spinner::new()).into_any_element(),
            State::Failed(message) => v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_1()
                .child(div().font_semibold().child("No Connections Found"))
                .child(div().text_sm().text_color(muted).text_center().max_w(px(420.)).child(message.clone()))
                .into_any_element(),
            State::Loaded(scan) if scan.connections.is_empty() && scan.skipped.is_empty() => v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .child(div().text_sm().text_color(muted).child("DBeaver has no saved connections here."))
                .into_any_element(),
            State::Loaded(scan) => {
                // Grouped by DBeaver folder (the scan is sorted by group, then name).
                let mut list = v_flex().id("import-list").size_full().gap_px().overflow_y_scrollbar();
                let mut group: Option<&str> = None;
                for (index, item) in scan.connections.iter().enumerate() {
                    if group != Some(item.config.group.as_str()) {
                        group = Some(item.config.group.as_str());
                        if !item.config.group.is_empty() {
                            list = list.child(
                                div().px_2().pt_3().pb_1().text_xs().font_semibold().text_color(muted).child(item.config.group.clone()),
                            );
                        }
                    }
                    list = list.child(self.render_row(index, item, cx));
                }
                if !scan.skipped.is_empty() {
                    list = list.child(div().px_2().pt_3().pb_1().text_xs().font_semibold().text_color(muted).child("Can’t Be Imported"));
                    for skipped in &scan.skipped {
                        list = list.child(
                            h_flex()
                                .px_2()
                                .py_1()
                                .gap_3()
                                .text_sm()
                                .child(div().truncate().child(skipped.name.clone()))
                                .child(div().flex_1())
                                .child(div().text_xs().text_color(muted).child(skipped.reason.clone())),
                        );
                    }
                }
                list.into_any_element()
            }
        };
        v_flex()
            .gap_2()
            .child(div().text_sm().text_color(muted).truncate().child(self.subtitle()))
            .child(div().h(px(380.)).border_1().border_color(border).rounded_md().p_1().child(body))
            .children(self.error.clone().map(|e| div().text_sm().text_color(red).child(e)))
    }
}
