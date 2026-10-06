//! "New Database…": a name, and the statement that creates it (from `dbcore`, like the macOS sheet).

use dbcore::dialect::Dialect;
use dbcore::{ConnectionConfig, DatabaseKind};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

pub struct NewDatabaseDialog {
    pub config: ConnectionConfig,
    name: Entity<InputState>,
    pub creating: bool,
    error: Option<String>,
    _subscription: Subscription,
}

/// What the dialog asks the workspace for (Enter in the name field).
pub struct CreateRequested;

impl EventEmitter<CreateRequested> for NewDatabaseDialog {}

impl NewDatabaseDialog {
    pub fn new(config: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("new_database"));
        name.update(cx, |input, cx| input.focus(window, cx));
        let subscription = cx.subscribe(&name, |_, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => cx.emit(CreateRequested),
            _ => cx.notify(),
        });
        Self { config, name, creating: false, error: None, _subscription: subscription }
    }

    pub fn name_input(&self) -> Entity<InputState> {
        self.name.clone()
    }

    pub fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_string()
    }

    fn kind(&self) -> DatabaseKind {
        self.config.kind
    }

    /// The statement, or why there isn't one yet (no name, or one the server can't take).
    pub fn statement(&self, cx: &App) -> Result<String, String> {
        Dialect(self.kind()).create_database(&self.name(cx)).map_err(|e| e.to_string())
    }

    pub fn set_creating(&mut self, creating: bool, cx: &mut Context<Self>) {
        self.creating = creating;
        if creating {
            self.error = None;
        }
        cx.notify();
    }

    pub fn show_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.creating = false;
        self.error = Some(message);
        cx.notify();
    }
}

impl Render for NewDatabaseDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let preview = match (self.name(cx).is_empty(), self.statement(cx)) {
            (true, _) => None,
            (false, Ok(sql)) => Some((sql, theme.foreground)),
            (false, Err(e)) => Some((e, theme.red)),
        };
        let address = match self.config.port {
            Some(port) => format!("{}:{port}", self.config.host),
            None => self.config.host.clone(),
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} · {address}", self.config.kind.display_name())),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(div().w(px(60.)).text_sm().text_color(theme.muted_foreground).child("Name"))
                    .child(div().flex_1().child(Input::new(&self.name))),
            )
            .children(preview.map(|(text, color)| {
                div()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .text_color(color)
                    .child(text)
            }))
            .children(self.error.clone().map(|e| div().text_sm().text_color(theme.red).child(e)))
    }
}
