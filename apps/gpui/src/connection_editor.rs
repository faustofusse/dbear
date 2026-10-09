//! The "New Connection" / "Edit Connection" form, shown in a dialog.
//! Validation, URLs and defaults come from `dbcore`, so it behaves like the macOS editor.

use std::sync::Arc;

use dbcore::secrets::{self, KeyringSecretStore};
use dbcore::{Connection, ConnectionConfig, DatabaseKind, SshAuth, SshTunnel, SslMode};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

const KINDS: [DatabaseKind; 5] =
    [DatabaseKind::Postgres, DatabaseKind::Mysql, DatabaseKind::SqlServer, DatabaseKind::Sqlite, DatabaseKind::Libsql];
const SSL_MODES: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

const SSH_AUTHS: [SshAuth; 3] = [SshAuth::Password, SshAuth::PrivateKey, SshAuth::Agent];

fn ssh_auth_title(auth: SshAuth) -> &'static str {
    match auth {
        SshAuth::Password => "Password",
        SshAuth::PrivateKey => "Private key",
        SshAuth::Agent => "SSH agent",
    }
}

/// What the form describes: the connection, and the secrets typed into it (`None`: untouched).
pub struct EditorValues {
    pub config: ConnectionConfig,
    pub password: Option<String>,
    /// The SSH password or key passphrase.
    pub ssh_secret: Option<String>,
}

fn ssl_title(mode: SslMode) -> &'static str {
    match mode {
        SslMode::Disable => "Disable",
        SslMode::Prefer => "Prefer",
        SslMode::Require => "Require",
        SslMode::VerifyFull => "Verify full",
    }
}

enum Status {
    Testing,
    Ok(String),
    Error(String),
}

pub struct ConnectionEditor {
    /// The connection being edited (`None`: a new one). Keeps the id and anything the form doesn't show.
    original: Option<ConnectionConfig>,
    kind: DatabaseKind,
    ssl_mode: SslMode,
    show_all_databases: bool,
    url: Entity<InputState>,
    name: Entity<InputState>,
    group: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    database: Entity<InputState>,
    /// Reach the server through an SSH server.
    use_ssh: bool,
    ssh_auth: SshAuth,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_key: Entity<InputState>,
    /// The SSH password or the key's passphrase.
    ssh_secret: Entity<InputState>,
    status: Option<Status>,
    secrets: Option<Arc<KeyringSecretStore>>,
    test_task: Option<Task<()>>,
}

impl ConnectionEditor {
    pub fn new(
        original: Option<ConnectionConfig>,
        secrets: Option<Arc<KeyringSecretStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = original.clone().unwrap_or_else(|| ConnectionConfig::new_empty(DatabaseKind::Postgres));
        let mut input = |value: String, placeholder: &str| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let url = input(String::new(), "postgres://user:password@host:5432/database");
        let name = input(config.name.clone(), "Optional");
        let group = input(config.group.clone(), "Optional, e.g. Production");
        let host = input(config.host.clone(), "localhost");
        let port = input(config.port.map(|p| p.to_string()).unwrap_or_default(), "");
        let user = input(config.user.clone().unwrap_or_default(), "");
        let database = input(config.database.clone(), "");
        // Saved passwords are never read back into the form (that could prompt for keychain access).
        let password_placeholder = if original.is_some() { "Unchanged" } else { "" };
        let ssh = config.ssh.clone().unwrap_or_default();
        let ssh_host = input(ssh.host.clone(), "bastion.example.com");
        let ssh_port = input(ssh.port.map(|p| p.to_string()).unwrap_or_default(), "22");
        let default_user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
        let ssh_user = input(if config.ssh.is_some() { ssh.user.clone() } else { default_user }, "Required");
        let ssh_key = input(ssh.key_path.clone(), "~/.ssh/id_ed25519");
        let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(password_placeholder));
        let secret_placeholder = if config.ssh.is_some() { "Unchanged" } else { "" };
        let ssh_secret = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(secret_placeholder));
        let mut editor = Self {
            original,
            kind: config.kind,
            ssl_mode: config.ssl_mode,
            show_all_databases: config.show_all_databases,
            url,
            name,
            group,
            host,
            port,
            user,
            password,
            database,
            use_ssh: config.ssh.is_some(),
            ssh_auth: ssh.auth,
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_key,
            ssh_secret,
            status: None,
            secrets,
            test_task: None,
        };
        editor.update_placeholders(window, cx);
        editor
    }

    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }

    fn update_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.kind;
        let port = kind.default_port().map(|p| p.to_string()).unwrap_or_default();
        self.port.update(cx, |s, cx| s.set_placeholder(port, window, cx));
        let database = match kind {
            DatabaseKind::Postgres => "Optional (postgres)",
            DatabaseKind::Sqlite => "/path/to/file.db",
            _ => "Optional",
        };
        self.database.update(cx, |s, cx| s.set_placeholder(database, window, cx));
        let host = if kind == DatabaseKind::Libsql { "mydb-org.turso.io" } else { "localhost" };
        self.host.update(cx, |s, cx| s.set_placeholder(host, window, cx));
    }

    fn set_kind(&mut self, kind: DatabaseKind, window: &mut Window, cx: &mut Context<Self>) {
        if kind == self.kind {
            return;
        }
        let defaults = ConnectionConfig::new_empty(kind);
        self.kind = kind;
        self.ssl_mode = defaults.ssl_mode;
        self.show_all_databases = defaults.show_all_databases;
        self.port.update(cx, |s, cx| s.set_value("", window, cx));
        if kind.is_sqlite_family() != self.host.read(cx).value().is_empty() {
            self.host.update(cx, |s, cx| s.set_value(defaults.host.clone(), window, cx));
        }
        self.status = None;
        self.update_placeholders(window, cx);
        cx.notify();
    }

    fn import_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        if url.is_empty() {
            return;
        }
        match ConnectionConfig::from_url(&url) {
            Ok(config) => {
                self.set_kind(config.kind, window, cx);
                self.kind = config.kind;
                self.ssl_mode = config.ssl_mode;
                self.show_all_databases = config.show_all_databases;
                let set = |input: &Entity<InputState>, value: String, window: &mut Window, cx: &mut Context<Self>| {
                    input.update(cx, |s, cx| s.set_value(value, window, cx));
                };
                if self.name.read(cx).value().is_empty() {
                    set(&self.name, config.name.clone(), window, cx);
                }
                set(&self.host, config.host.clone(), window, cx);
                set(&self.port, config.port.map(|p| p.to_string()).unwrap_or_default(), window, cx);
                set(&self.user, config.user.clone().unwrap_or_default(), window, cx);
                set(&self.password, config.password.clone().unwrap_or_default(), window, cx);
                set(&self.database, config.database.clone(), window, cx);
                set(&self.url, String::new(), window, cx);
                self.update_placeholders(window, cx);
                self.status = None;
            }
            Err(e) => self.status = Some(Status::Error(e.to_string())),
        }
        cx.notify();
    }

    fn choose_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            this.update_in(cx, |this, window, cx| {
                this.ssh_key.update(cx, |s, cx| s.set_value(path.display().to_string(), window, cx));
            })
            .ok();
        })
        .detach();
    }

    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            this.update_in(cx, |this, window, cx| {
                this.database.update(cx, |s, cx| s.set_value(path.display().to_string(), window, cx));
            })
            .ok();
        })
        .detach();
    }

    /// What the form describes, validated. Passwords are only included when typed.
    pub fn values(&self, cx: &App) -> Result<EditorValues, String> {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let mut config = self.original.clone().unwrap_or_else(|| ConnectionConfig::new_empty(self.kind));
        config.kind = self.kind;
        config.name = text(&self.name);
        config.group = text(&self.group);
        config.host = text(&self.host);
        let port = text(&self.port);
        config.port = if port.is_empty() {
            None
        } else {
            Some(port.parse::<u16>().map_err(|_| "Port must be between 1 and 65535.".to_string())?)
        };
        let user = text(&self.user);
        config.user = (!user.is_empty()).then_some(user);
        config.database = text(&self.database);
        config.ssl_mode = self.ssl_mode;
        config.show_all_databases = self.show_all_databases;
        config.password = None;
        if self.kind == DatabaseKind::Sqlite {
            config.host.clear();
            config.port = None;
            config.user = None;
        }
        config.ssh = None;
        if self.use_ssh && config.supports_ssh() {
            let port = text(&self.ssh_port);
            config.ssh = Some(SshTunnel {
                host: text(&self.ssh_host),
                port: if port.is_empty() {
                    None
                } else {
                    Some(port.parse::<u16>().map_err(|_| "SSH port must be between 1 and 65535.".to_string())?)
                },
                user: text(&self.ssh_user),
                auth: self.ssh_auth,
                key_path: if self.ssh_auth == SshAuth::PrivateKey { text(&self.ssh_key) } else { String::new() },
                ..Default::default()
            });
        }
        config.validate().map_err(|e| e.to_string())?;
        // Not trimmed: spaces can be part of a password.
        let typed = |input: &Entity<InputState>| Some(input.read(cx).value().to_string()).filter(|p| !p.is_empty());
        let ssh_secret = config.ssh.as_ref().filter(|s| s.auth != SshAuth::Agent).and_then(|_| typed(&self.ssh_secret));
        Ok(EditorValues { config, password: typed(&self.password), ssh_secret })
    }

    pub fn show_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = Some(Status::Error(message));
        cx.notify();
    }

    pub fn test(&mut self, cx: &mut Context<Self>) {
        let EditorValues { mut config, password, ssh_secret } = match self.values(cx) {
            Ok(values) => values,
            Err(e) => return self.show_error(e, cx),
        };
        // Editing without retyping a password: test with the saved one.
        let saved = if !self.is_new() { self.secrets.clone() } else { None };
        config.password = password;
        if let Some(ssh) = config.ssh.as_mut() {
            ssh.secret = ssh_secret;
        }
        self.status = Some(Status::Testing);
        self.test_task = Some(cx.spawn(async move |this, cx| {
            let config = cx
                .background_executor()
                .spawn(async move {
                    match saved {
                        Some(store) => secrets::with_password(store.as_ref(), config.clone()).unwrap_or(config),
                        None => config,
                    }
                })
                .await;
            let connection = Connection::new(config);
            let result = connection.connect().await;
            connection.disconnect().await;
            this.update(cx, |this, cx| {
                this.status = Some(match result {
                    Ok(()) => Status::Ok("Connected".into()),
                    Err(e) => Status::Error(e.to_string()),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Connecting…", "Connected" or why not, for the dialog's footer: the form can be taller than
    /// the dialog, and the result of Test Connection (or a failed save) must be in view.
    pub fn status_line(&self, cx: &App) -> Option<impl IntoElement + use<>> {
        let (text, color) = match self.status.as_ref()? {
            Status::Testing => ("Connecting…".to_string(), cx.theme().muted_foreground),
            Status::Ok(text) => (text.clone(), cx.theme().green),
            Status::Error(text) => (text.clone(), cx.theme().red),
        };
        let tooltip = SharedString::from(text.clone());
        Some(
            div()
                .id("editor-status")
                .min_w_0()
                .flex_1()
                .text_sm()
                .text_color(color)
                .line_clamp(2)
                .child(text)
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)),
        )
    }

    fn field(label: &str, input: impl IntoElement, cx: &App) -> impl IntoElement {
        h_flex()
            .gap_3()
            .child(div().w(px(110.)).flex_shrink_0().text_sm().text_color(cx.theme().muted_foreground).child(label.to_string()))
            .child(div().flex_1().min_w_0().child(input))
    }
}

impl Render for ConnectionEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let kind = self.kind;
        let kind_menu = Button::new("kind").outline().label(kind.display_name()).dropdown_caret(true).dropdown_menu({
            let this = this.clone();
            move |mut menu, _, _| {
                for k in KINDS {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(k.display_name()).checked(k == kind).on_click(
                        move |_, window, cx| {
                            this.update(cx, |e, cx| e.set_kind(k, window, cx)).ok();
                        },
                    ));
                }
                menu
            }
        });
        let ssl_mode = self.ssl_mode;
        let ssl_menu = Button::new("ssl").outline().label(ssl_title(ssl_mode)).dropdown_caret(true).dropdown_menu({
            let this = this.clone();
            move |mut menu, _, _| {
                for mode in SSL_MODES {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(ssl_title(mode)).checked(mode == ssl_mode).on_click(
                        move |_, _, cx| {
                            this.update(cx, |e, cx| {
                                e.ssl_mode = mode;
                                cx.notify();
                            })
                            .ok();
                        },
                    ));
                }
                menu
            }
        });

        let sqlite = kind == DatabaseKind::Sqlite;
        let libsql = kind == DatabaseKind::Libsql;

        v_flex()
            .gap_3()
            .child(Self::field(
                "Import URL",
                h_flex().gap_2().child(div().flex_1().child(Input::new(&self.url))).child(
                    Button::new("import").outline().label("Import").on_click(cx.listener(|this, _, window, cx| this.import_url(window, cx))),
                ),
                cx,
            ))
            .child(div().h_px().bg(cx.theme().border))
            .child(Self::field("Type", h_flex().child(kind_menu), cx))
            .child(Self::field("Name", Input::new(&self.name), cx))
            .child(Self::field("Group", Input::new(&self.group), cx))
            .when(sqlite, |form| {
                form.child(Self::field(
                    "File",
                    h_flex().gap_2().child(div().flex_1().child(Input::new(&self.database))).child(
                        Button::new("choose").outline().label("Choose…").on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
                    ),
                    cx,
                ))
            })
            .when(!sqlite, |form| {
                form.child(Self::field("Host", Input::new(&self.host), cx))
                    .child(Self::field("Port", div().w(px(120.)).child(Input::new(&self.port)), cx))
                    .when(!libsql, |form| form.child(Self::field("User", Input::new(&self.user), cx)))
                    .child(Self::field(if libsql { "Auth token" } else { "Password" }, Input::new(&self.password).mask_toggle(), cx))
                    .when(!libsql, |form| form.child(Self::field("Database", Input::new(&self.database), cx)))
                    .child(Self::field("SSL", h_flex().child(ssl_menu), cx))
            })
            .when(ConnectionConfig::new_empty(kind).supports_ssh(), |form| {
                let auth = self.ssh_auth;
                let auth_menu = Button::new("ssh-auth").outline().label(ssh_auth_title(auth)).dropdown_caret(true).dropdown_menu({
                    let this = this.clone();
                    move |mut menu, _, _| {
                        for a in SSH_AUTHS {
                            let this = this.clone();
                            menu = menu.item(PopupMenuItem::new(ssh_auth_title(a)).checked(a == auth).on_click(move |_, _, cx| {
                                this.update(cx, |e, cx| {
                                    e.ssh_auth = a;
                                    cx.notify();
                                })
                                .ok();
                            }));
                        }
                        menu
                    }
                });
                form.child(div().h_px().bg(cx.theme().border))
                    .child(Self::field(
                        "SSH tunnel",
                        Switch::new("use-ssh").small().checked(self.use_ssh).label("Connect through an SSH server").on_click(
                            cx.listener(|this, checked: &bool, _, cx| {
                                this.use_ssh = *checked;
                                cx.notify();
                            }),
                        ),
                        cx,
                    ))
                    .when(self.use_ssh, |form| {
                        form.child(Self::field("SSH host", Input::new(&self.ssh_host), cx))
                            .child(Self::field("SSH port", div().w(px(120.)).child(Input::new(&self.ssh_port)), cx))
                            .child(Self::field("SSH user", Input::new(&self.ssh_user), cx))
                            .child(Self::field("Sign in with", h_flex().child(auth_menu), cx))
                            .when(auth == SshAuth::PrivateKey, |form| {
                                form.child(Self::field(
                                    "Key file",
                                    h_flex().gap_2().child(div().flex_1().child(Input::new(&self.ssh_key))).child(
                                        Button::new("choose-key")
                                            .outline()
                                            .label("Choose…")
                                            .on_click(cx.listener(|this, _, window, cx| this.choose_key(window, cx))),
                                    ),
                                    cx,
                                ))
                            })
                            .when(auth != SshAuth::Agent, |form| {
                                let label = if auth == SshAuth::PrivateKey { "Passphrase" } else { "SSH password" };
                                form.child(Self::field(label, Input::new(&self.ssh_secret).mask_toggle(), cx))
                            })
                            .child(Self::field(
                                "",
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Host and port above are as seen from the SSH server."),
                                cx,
                            ))
                    })
            })
            .when(ConnectionConfig::new_empty(kind).supports_multiple_databases(), |form| {
                form.child(Self::field(
                    "",
                    Switch::new("show-all").small().checked(self.show_all_databases).label("Show all databases").on_click(
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.show_all_databases = *checked;
                            cx.notify();
                        }),
                    ),
                    cx,
                ))
            })
    }
}
