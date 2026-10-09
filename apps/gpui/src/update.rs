//! Self-update, on platforms that have an installer for it (Windows today; elsewhere this is a
//! no-op, and Linux can plug in an [`Installer`] later).
//!
//! The release core is the `dbear-update` crate: a signed manifest (ed25519, key compiled in from
//! `DBEAR_UPDATE_PUBLIC_KEY`) names each artifact's size, SHA-256 and signature. Nothing is
//! installed unless all of them match. Builds without a key never check.
//!
//! Like the macOS app: it checks at launch and then once a day, downloads in the background and
//! installs when you quit; **Restart to Update** installs right away. Both can be turned off in the
//! Updates dialog (Help ▸ Check for Updates…, or the button at the bottom of the connections
//! column); the choice is kept in `state.db` (`gpui.updates`).
//!
//! Installed copies update by running the new installer. Portable copies (the zip) replace their
//! own `dbear.exe` in place when their folder is writable (`dbear_update::replace`), so they need
//! no installer or administrator rights either; in a folder they can't write to they only notify.
//!
//! Command line (also used by `scripts/test-update-windows.sh`):
//! - `dbear --version`
//! - `dbear --update`: check, download, verify and install (start the installer, or replace a
//!   portable copy's exe), without a window. Exit code 0 when done or started, 3 when already up
//!   to date, 1 on errors.
//!
//! `DBEAR_UPDATE_FEED` points at another manifest (an `http(s)://` or `file://` URL), e.g. a
//! local test feed; it must still be signed with the compiled-in key.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbcore::state::StateStore;
use dbear_update::{PublicKey, Update};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

/// This build's version: the release tag's (CI sets `DBEAR_VERSION`) or the crate's.
pub const VERSION: &str = env!("DBEAR_VERSION");
const PUBLIC_KEYS: &str = env!("DBEAR_UPDATE_PUBLIC_KEY");
const RELEASES_URL: &str = "https://github.com/faustofusse/dbear/releases";
/// Key of the settings in `state.db`.
const SETTINGS_KEY: &str = "gpui.updates";
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// After launch, so checking doesn't compete with opening the window and the last session.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(5);

actions!(dbear, [CheckForUpdates, RestartToUpdate]);

// MARK: Platforms

/// Installs a verified download. One per platform: the Windows one runs the NSIS installer.
pub trait Installer: Send + Sync {
    /// The manifest artifact `kind` it installs (`nsis`).
    fn kind(&self) -> &'static str;
    /// Starts installing `file` once this process exits; `relaunch` opens the new version after.
    /// The caller quits right after.
    fn install(&self, file: &Path, relaunch: bool) -> std::io::Result<()>;
}

/// How this copy of the app can be updated.
enum Mode {
    /// Downloads and installs updates itself.
    Install(Box<dyn Installer>),
    /// Only says there's a new version (e.g. a portable copy, which has no installer to run).
    NotifyOnly(&'static str),
}

/// Manifests per platform family, as assets of the latest GitHub release.
fn default_feed() -> Option<String> {
    let family = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "macos"
    };
    Some(format!("{RELEASES_URL}/latest/download/dbear-update-{family}.json"))
}

fn platform_mode() -> Option<Mode> {
    // Debug builds can try the whole flow anywhere with a test feed; installing only logs.
    // `DBEAR_UPDATE_DRY_RUN=portable` acts like a portable copy.
    if let Some(value) = std::env::var_os("DBEAR_UPDATE_DRY_RUN").filter(|_| cfg!(debug_assertions)) {
        return Some(if value == "portable" { Mode::NotifyOnly(PORTABLE) } else { Mode::Install(Box::new(DryRun)) });
    }
    #[cfg(windows)]
    {
        windows::mode()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

const PORTABLE: &str = "dbear can’t update itself here because it can’t write to its folder. Download the new version, or move dbear.exe to a folder of yours (e.g. in Documents) to get updates automatically.";

struct DryRun;

impl Installer for DryRun {
    fn kind(&self) -> &'static str {
        "nsis"
    }
    fn install(&self, file: &Path, relaunch: bool) -> std::io::Result<()> {
        log::warn!("dry run: would install {} (relaunch: {relaunch})", file.display());
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::{Installer, Mode};

    /// The NSIS installer (`packaging/windows/dbear.nsi`) in update mode: silent, into this
    /// copy's folder, waiting for this process to exit first.
    struct Nsis {
        dir: PathBuf,
    }

    pub(super) fn mode() -> Option<Mode> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        // The installer puts its uninstaller beside the app; the portable zip has none. A portable
        // copy replaces its own exe, which only needs its folder to be writable.
        if dir.join("uninstall.exe").is_file() {
            Some(Mode::Install(Box::new(Nsis { dir })))
        } else if dbear_update::replace::can_replace(&exe) {
            Some(Mode::Install(Box::new(Portable { exe })))
        } else {
            Some(Mode::NotifyOnly(super::PORTABLE))
        }
    }

    /// The portable zip: swaps the running exe for the zip's (see `dbear_update::replace`). The
    /// old one is removed at the next launch (`super::remove_previous`).
    struct Portable {
        exe: PathBuf,
    }

    impl Installer for Portable {
        fn kind(&self) -> &'static str {
            "zip"
        }

        fn install(&self, file: &Path, relaunch: bool) -> std::io::Result<()> {
            dbear_update::replace::replace_from_zip(file, "dbear.exe", &self.exe)?;
            if relaunch {
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                // `--updated`: the new process waits for this one to exit before it starts.
                Command::new(&self.exe)
                    .arg(super::UPDATED_ARG)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(DETACHED_PROCESS)
                    .spawn()?;
            }
            Ok(())
        }
    }

    impl Installer for Nsis {
        fn kind(&self) -> &'static str {
            "nsis"
        }

        fn install(&self, file: &Path, relaunch: bool) -> std::io::Result<()> {
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            let mut command = Command::new(file);
            command.arg("/S").arg("/UPDATE");
            if relaunch {
                command.arg("/RELAUNCH");
            }
            // NSIS wants `/D=` last and unquoted, even with spaces.
            command.raw_arg(format!("/D={}", self.dir.display()));
            command
                .current_dir(file.parent().unwrap_or(Path::new(".")))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .map(drop)
        }
    }
}

// MARK: Configuration

struct Config {
    keys: Vec<PublicKey>,
    feed: String,
    target: String,
    mode: Mode,
}

impl Config {
    /// `None` when this build doesn't update itself (no key, or no installer for the platform).
    fn load() -> Option<Self> {
        let keys = match dbear_update::parse_public_keys(PUBLIC_KEYS) {
            Ok(keys) if !keys.is_empty() => keys,
            Ok(_) => return None,
            Err(e) => {
                log::error!("the update key compiled into this build is invalid: {e}");
                return None;
            }
        };
        let mode = platform_mode()?;
        let feed = std::env::var("DBEAR_UPDATE_FEED").ok().filter(|f| !f.is_empty()).or_else(default_feed)?;
        Some(Config { keys, feed, target: dbear_update::current_target(), mode })
    }

    /// The artifact kind to look for. A copy that only notifies still wants to know an update
    /// exists, whatever it's packaged as.
    fn kind(&self) -> &'static str {
        match &self.mode {
            Mode::Install(installer) => installer.kind(),
            Mode::NotifyOnly(_) => "nsis",
        }
    }

    fn downloads_dir() -> PathBuf {
        std::env::temp_dir().join("dbear-updates")
    }
}

/// Saved in `state.db`.
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(default)]
struct Settings {
    /// Check at launch and once a day.
    auto_check: bool,
    /// Download updates when found and install them on quit.
    auto_install: bool,
    /// Milliseconds since the Unix epoch.
    last_check: Option<i64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_check: true, auto_install: true, last_check: None }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn state_store() -> Option<StateStore> {
    let store = std::env::var_os("DBEAR_CONNECTIONS_FILE").map(PathBuf::from).or_else(dbcore::store::default_path)?;
    StateStore::open_beside(&store).map_err(|e| log::warn!("couldn’t open the state file for update settings: {e}")).ok()
}

// MARK: Updater

#[derive(Clone)]
enum Status {
    Idle,
    Checking,
    UpToDate,
    /// Found; waiting for the user (automatic installs off, or a portable copy).
    Available(Arc<Update>),
    Downloading { update: Arc<Update>, done: Arc<AtomicU64>, cancel: Arc<AtomicBool> },
    /// Downloaded and verified.
    Ready { update: Arc<Update>, file: PathBuf },
    Failed(String),
}

pub struct Updater {
    config: Config,
    settings: Settings,
    state: Option<StateStore>,
    status: Status,
    /// The installer was started (Restart to Update); quitting mustn't start it again.
    installing: bool,
    _check: Option<Task<()>>,
    _download: Option<Task<()>>,
    _schedule: Task<()>,
    _quit: Subscription,
}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

/// Sets up updates when this build has them; call once at startup.
pub fn init(cx: &mut App) {
    let Some(config) = Config::load() else {
        return;
    };
    let updater = cx.new(|cx| Updater::new(config, cx));
    cx.set_global(GlobalUpdater(updater));
    cx.on_action(|_: &CheckForUpdates, cx| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, cx| open_dialog(true, window, cx)).ok();
        }
    });
    cx.on_action(|_: &RestartToUpdate, cx| {
        if let Some(updater) = updater_entity(cx) {
            updater.update(cx, |u, cx| u.restart_to_update(cx));
        }
    });
    // Debug builds: `DBEAR_UPDATE_SHOW_DIALOG=1` opens the dialog and checks, for screenshots.
    if cfg!(debug_assertions) && std::env::var_os("DBEAR_UPDATE_SHOW_DIALOG").is_some() {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            cx.update(|cx| {
                if let Some(window) = cx.windows().first() {
                    window.update(cx, |_, window, cx| open_dialog(true, window, cx)).ok();
                }
            })
        })
        .detach();
    }
}

/// Whether this build updates itself (it has a key and an installer for the platform).
pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| Config::load().is_some())
}

fn updater_entity(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<GlobalUpdater>().map(|g| g.0.clone())
}

impl Updater {
    fn new(config: Config, cx: &mut Context<Self>) -> Self {
        let state = state_store();
        let settings = state
            .as_ref()
            .and_then(|s| s.get(SETTINGS_KEY))
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let schedule = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_AFTER).await;
            let mut first = true;
            loop {
                let Ok(due) = this.update(cx, |u, _| {
                    let since = u.settings.last_check.map_or(i64::MAX, |t| now_ms() - t);
                    // At launch, unless the dialog just checked.
                    let every = if first { 60_000 } else { CHECK_EVERY.as_millis() as i64 };
                    u.settings.auto_check && since >= every
                }) else {
                    return;
                };
                if due {
                    this.update(cx, |u, cx| u.check(false, cx)).ok();
                }
                first = false;
                cx.background_executor().timer(Duration::from_secs(60 * 60)).await;
            }
        });
        // Automatic installs happen when the app quits, like Sparkle on macOS.
        let quit = cx.on_app_quit(|this, _| {
            if let (Status::Ready { file, .. }, Mode::Install(installer)) = (&this.status, &this.config.mode) {
                if this.settings.auto_install && !this.installing {
                    match installer.install(file, false) {
                        Ok(()) => this.installing = true,
                        Err(e) => log::error!("couldn’t start the update installer: {e}"),
                    }
                }
            }
            async {}
        });
        Updater { config, settings, state, status: Status::Idle, installing: false, _check: None, _download: None, _schedule: schedule, _quit: quit }
    }

    fn save_settings(&mut self) {
        if let Some(state) = &mut self.state {
            let json = serde_json::to_string(&self.settings).expect("settings serialize");
            if let Err(e) = state.set(SETTINGS_KEY, Some(&json)) {
                log::warn!("couldn’t save the update settings: {e}");
            }
        }
    }

    fn busy(&self) -> bool {
        matches!(self.status, Status::Checking | Status::Downloading { .. })
    }

    /// `manual`: from the dialog, which shows every outcome. Background checks stay quiet unless
    /// they find something.
    fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.busy() || matches!(self.status, Status::Ready { .. }) {
            return;
        }
        let previous = std::mem::replace(&mut self.status, Status::Checking);
        cx.notify();
        let (feed, keys, target, kind) = (self.config.feed.clone(), self.config.keys.clone(), self.config.target.clone(), self.config.kind());
        let job = cx.background_spawn(async move { dbear_update::check(&feed, &keys, VERSION, &target, kind) });
        self._check = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |u, cx| {
                u.settings.last_check = Some(now_ms());
                u.save_settings();
                match result {
                    Ok(Some(update)) => {
                        log::info!("dbear {} is available", update.version());
                        let update = Arc::new(update);
                        u.status = Status::Available(update.clone());
                        if u.settings.auto_install && matches!(u.config.mode, Mode::Install(_)) {
                            u.download(update, cx);
                        }
                    }
                    Ok(None) => {
                        u.status = Status::UpToDate;
                        let _ = std::fs::remove_dir_all(Config::downloads_dir());
                    }
                    Err(e) => {
                        log::warn!("update check failed: {e}");
                        u.status = match (manual, e) {
                            (true, dbear_update::Error::NoFeed(_)) => Status::Failed("No update information was found for this platform.".into()),
                            (true, e) => Status::Failed(e.to_string()),
                            // Keep showing what was known (e.g. an update to download).
                            (false, _) => match previous {
                                Status::Available(_) => previous,
                                _ => Status::Idle,
                            },
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn download(&mut self, update: Arc<Update>, cx: &mut Context<Self>) {
        if !matches!(self.config.mode, Mode::Install(_)) || matches!(self.status, Status::Downloading { .. }) {
            return;
        }
        let done = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        self.status = Status::Downloading { update: update.clone(), done: done.clone(), cancel: cancel.clone() };
        cx.notify();
        let keys = self.config.keys.clone();
        let job = cx.background_spawn({
            let (update, done) = (update.clone(), done.clone());
            async move {
                dbear_update::download(&update, &Config::downloads_dir(), &keys, &cancel, |d, _| done.store(d, Ordering::Relaxed))
            }
        });
        self._download = Some(cx.spawn(async move |this, cx| {
            // Progress: redraw a few times a second while it runs.
            let ticker = cx.spawn({
                let this = this.clone();
                async move |cx| loop {
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            });
            let result = job.await;
            drop(ticker);
            this.update(cx, |u, cx| {
                u.status = match result {
                    Ok(file) => {
                        log::info!("dbear {} downloaded and verified: {}", update.version(), file.display());
                        Status::Ready { update, file }
                    }
                    Err(dbear_update::Error::Cancelled) => Status::Available(update),
                    Err(e) => {
                        log::error!("update download failed: {e}");
                        Status::Failed(format!("Couldn’t download dbear {}: {e}", update.version()))
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn cancel_download(&mut self) {
        if let Status::Downloading { cancel, .. } = &self.status {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let (Status::Ready { file, .. }, Mode::Install(installer)) = (&self.status, &self.config.mode) else {
            return;
        };
        match installer.install(file, true) {
            Ok(()) => {
                self.installing = true;
                cx.quit();
            }
            Err(e) => {
                self.status = Status::Failed(format!("Couldn’t start the installer: {e}"));
                cx.notify();
            }
        }
    }

    fn set_auto_check(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.auto_check = on;
        self.save_settings();
        cx.notify();
    }

    fn set_auto_install(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.auto_install = on;
        self.save_settings();
        if let (true, Status::Available(update)) = (on, &self.status) {
            let update = update.clone();
            self.download(update, cx);
        }
        cx.notify();
    }
}

// MARK: UI

/// The button at the bottom of the connections column: opens the Updates dialog, and says when an
/// update is ready. Nothing when this build doesn't update itself.
pub fn footer_button(cx: &App) -> AnyElement {
    let Some(updater) = updater_entity(cx) else {
        return Empty.into_any_element();
    };
    let u = updater.read(cx);
    let open = |_: &ClickEvent, window: &mut Window, cx: &mut App| open_dialog(false, window, cx);
    match &u.status {
        Status::Ready { update, .. } => Button::new("update-ready")
            .primary()
            .xsmall()
            .label("Update")
            .tooltip(format!("dbear {} is ready: restart to update", update.version()))
            .on_click(open)
            .into_any_element(),
        Status::Available(update) => Button::new("update-available")
            .ghost()
            .small()
            .icon(Icon::new(AssetIcon::CircleArrowUp).text_color(cx.theme().primary))
            .tooltip(format!("dbear {} is available", update.version()))
            .on_click(open)
            .into_any_element(),
        Status::Downloading { update, .. } => Button::new("update-downloading")
            .ghost()
            .small()
            .icon(Icon::new(AssetIcon::CircleArrowUp))
            .loading(true)
            .tooltip(format!("Downloading dbear {}…", update.version()))
            .on_click(open)
            .into_any_element(),
        _ => Button::new("updates").ghost().small().icon(Icon::new(AssetIcon::CircleArrowUp)).tooltip("Updates…").on_click(open).into_any_element(),
    }
}

/// The Updates dialog; `check` starts a check right away (Check for Updates…).
pub fn open_dialog(check: bool, window: &mut Window, cx: &mut App) {
    let Some(updater) = updater_entity(cx) else {
        return;
    };
    if check {
        updater.update(cx, |u, cx| u.check(true, cx));
    }
    let view = cx.new(|cx| UpdatesView::new(updater, cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Updates").w(px(460.)).child(view.clone()));
}

struct UpdatesView {
    updater: Entity<Updater>,
    _observe: Subscription,
}

impl UpdatesView {
    fn new(updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&updater, |_, _, cx| cx.notify());
        UpdatesView { updater, _observe: observe }
    }
}

impl Render for UpdatesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let updater = self.updater.clone();
        let u = updater.read(cx);
        let settings = u.settings;
        let notify_only = match &u.config.mode {
            Mode::NotifyOnly(why) => Some(*why),
            Mode::Install(_) => None,
        };
        let checked_at = settings.last_check.map(|t| format!("Last checked {}.", ago(now_ms() - t)));

        let (message, color): (String, Hsla) = match &u.status {
            Status::Idle => (checked_at.clone().unwrap_or_else(|| "Not checked yet.".into()), theme.muted_foreground),
            Status::Checking => ("Checking for updates…".into(), theme.muted_foreground),
            Status::UpToDate => (format!("dbear {VERSION} is the latest version."), theme.foreground),
            Status::Available(update) => (format!("dbear {} is available.", update.version()), theme.foreground),
            Status::Downloading { update, .. } => (format!("Downloading dbear {}…", update.version()), theme.foreground),
            Status::Ready { update, .. } => (
                if settings.auto_install {
                    format!("dbear {} is ready. It installs when you quit, or restart now.", update.version())
                } else {
                    format!("dbear {} is ready to install.", update.version())
                },
                theme.foreground,
            ),
            Status::Failed(e) => (e.clone(), theme.red),
        };
        let update = match &u.status {
            Status::Available(update) | Status::Downloading { update, .. } | Status::Ready { update, .. } => Some(update.clone()),
            _ => None,
        };
        let progress = match &u.status {
            Status::Downloading { update, done, .. } => {
                let total = update.artifact.size.max(1);
                Some(done.load(Ordering::Relaxed) as f32 / total as f32 * 100.)
            }
            _ => None,
        };

        let action = |id: &'static str, label: &'static str, f: fn(&mut Updater, &mut Context<Updater>)| {
            let updater = updater.clone();
            Button::new(id).label(label).small().on_click(move |_, _, cx| updater.update(cx, f))
        };
        let mut buttons = h_flex().gap_2().justify_end();
        match &u.status {
            Status::Ready { .. } => buttons = buttons.child(action("restart", "Restart to Update", |u, cx| u.restart_to_update(cx)).primary()),
            Status::Downloading { .. } => buttons = buttons.child(action("cancel", "Cancel", |u, _| u.cancel_download())),
            Status::Available(update) if notify_only.is_some() => {
                let url = update.manifest.notes_url.clone().unwrap_or_else(|| format!("{RELEASES_URL}/latest"));
                buttons = buttons.child(Button::new("download").label("Download…").small().primary().on_click(move |_, _, cx| cx.open_url(&url)));
            }
            Status::Available(_) => {
                buttons = buttons.child(action("install", "Download and Install", |u, cx| {
                    if let Status::Available(update) = &u.status {
                        let update = update.clone();
                        u.download(update, cx);
                    }
                }).primary())
            }
            _ => {}
        }
        if !matches!(u.status, Status::Ready { .. } | Status::Downloading { .. } | Status::Available(_)) {
            buttons = buttons.child(action("check", "Check Now", |u, cx| u.check(true, cx)).disabled(u.busy()).loading(matches!(u.status, Status::Checking)));
        }

        v_flex()
            .gap_3()
            .text_sm()
            .child(div().text_color(theme.muted_foreground).child(format!("You have dbear {VERSION}.")))
            .child(div().text_color(color).child(message))
            .children(progress.map(|value| Progress::new("update-progress").value(value)))
            .children(update.as_ref().and_then(|u| u.manifest.notes.clone()).map(|notes| {
                div()
                    .id("update-notes")
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(TextView::markdown("update-notes-text", notes))
            }))
            .children(update.as_ref().and_then(|u| u.manifest.notes_url.clone()).map(|url| {
                h_flex().child(Button::new("notes").link().xsmall().label("Release notes").on_click(move |_, _, cx| cx.open_url(&url)))
            }))
            .child(buttons)
            .children(notify_only.map(|why| div().text_xs().text_color(theme.muted_foreground).child(why)))
            .child(
                v_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Checkbox::new("auto-check").checked(settings.auto_check).label("Check for updates automatically").on_click({
                        let updater = updater.clone();
                        move |on, _, cx| updater.update(cx, |u, cx| u.set_auto_check(*on, cx))
                    }))
                    .when(notify_only.is_none(), |list| {
                        list.child(
                            Checkbox::new("auto-install")
                                .checked(settings.auto_install)
                                .label("Download updates and install them when dbear quits")
                                .on_click({
                                    let updater = updater.clone();
                                    move |on, _, cx| updater.update(cx, |u, cx| u.set_auto_install(*on, cx))
                                }),
                        )
                    }),
            )
    }
}

fn ago(ms: i64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        ..1 => "just now".into(),
        1 => "a minute ago".into(),
        2..60 => format!("{minutes} minutes ago"),
        60..120 => "an hour ago".into(),
        120..1440 => format!("{} hours ago", minutes / 60),
        1440..2880 => "yesterday".into(),
        _ => format!("{} days ago", minutes / 1440),
    }
}

// MARK: Command line

/// Handles `--version` and `--update`; `Some(exit code)` when it did (no window is opened).
pub fn run_cli() -> Option<i32> {
    remove_previous();
    let arg = std::env::args().nth(1)?;
    match arg.as_str() {
        "--version" | "-V" => {
            println!("dbear {VERSION}");
            Some(0)
        }
        "--update" => Some(update_now()),
        // Anything else, `--updated` included, is a normal launch.
        _ => None,
    }
}

/// Passed to a portable copy started right after it updated itself.
const UPDATED_ARG: &str = "--updated";

/// Removes what a portable copy's last update left beside it (`dbear.old.exe`). After a restart to
/// update, the previous process may still be quitting (saving the open tabs): wait for it, so this
/// one starts with its tabs.
fn remove_previous() {
    let Ok(exe) = std::env::current_exe() else { return };
    let restarted = std::env::args().nth(1).as_deref() == Some(UPDATED_ARG);
    let wait = if restarted { Duration::from_secs(30) } else { Duration::ZERO };
    if !dbear_update::replace::remove_old(&exe, wait) {
        log::warn!("couldn’t remove {}", dbear_update::replace::old_path(&exe).display());
    }
}

fn update_now() -> i32 {
    let Some(config) = Config::load() else {
        eprintln!("this build of dbear doesn’t update itself");
        return 1;
    };
    let Mode::Install(installer) = &config.mode else {
        eprintln!("this copy of dbear can’t update itself (portable, in a folder it can’t write to)");
        return 1;
    };
    println!("dbear {VERSION}: checking {}", config.feed);
    let update = match dbear_update::check(&config.feed, &config.keys, VERSION, &config.target, installer.kind()) {
        Ok(Some(update)) => update,
        Ok(None) => {
            println!("up to date");
            return 3;
        }
        Err(e) => {
            eprintln!("update check failed: {e}");
            return 1;
        }
    };
    println!("downloading dbear {} from {}", update.version(), update.artifact.url);
    let file = match dbear_update::download(&update, &Config::downloads_dir(), &config.keys, &AtomicBool::new(false), |_, _| {}) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("download failed: {e}");
            return 1;
        }
    };
    println!("verified {}; installing ({})", file.display(), installer.kind());
    match installer.install(&file, false) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("couldn’t start the installer: {e}");
            1
        }
    }
}
