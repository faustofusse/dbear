//! dbear for Linux, written with GPUI. It also runs on macOS and Windows, which is handy for
//! development; the macOS release is the SwiftUI app in `apps/macos`.

mod assets;
mod backup;
mod connection_editor;
mod grid;
mod inspector;
mod keys;
mod menus;
mod new_database;
mod highlight;
mod import_dialog;
mod sql_complete;
mod tabs;
mod users;
mod workspace;

use gpui_kit::component::Theme;
use gpui_kit::*;

actions!(dbear, [Quit]);

fn main() {
    // RUST_LOG=info (or debug) shows what GPUI and the drivers are doing; warnings and errors by default.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    gpui_kit::application().with_assets(assets::AppAssets).run(|cx| {
        gpui_kit::init(cx);
        Theme::sync_system_appearance(None, cx);
        // Selected cells and rows: a soft neutral highlight, like the macOS app (not the theme's blue).
        Theme::update(cx, |theme| {
            let fg = theme.colors.foreground;
            theme.colors.table_active = fg.opacity(0.10);
            theme.colors.table_active_border = fg.opacity(0.25);
        });
        // Scrollbars follow the system setting (on macOS usually "when scrolling"). The grid and the
        // inspector add an always-visible horizontal one of their own.
        Theme::sync_scrollbar_appearance(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_global(inspector::ShowInspector(false));
        cx.on_action(|_: &tabs::ToggleInspector, cx| inspector::toggle(cx));
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        workspace::bind_keys(cx);
        tabs::bind_keys(cx);
        inspector::bind_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("dbear".into()), ..Default::default() }),
            window_min_size: Some(size(px(800.), px(480.))),
            app_id: Some("ar.fausto.dbear".into()),
            ..Default::default()
        };
        let (window, workspace) =
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(window, cx)))
                .expect("failed to open the window");
        // Reopen last session's tabs once the window exists.
        window
            .update(cx, |_, window, cx| workspace.update(cx, |w, cx| w.restore_session(window, cx)))
            .ok();
        cx.activate(true);
    });
}
