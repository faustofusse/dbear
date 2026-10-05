//! dbear for Linux, written with GPUI. It also runs on macOS and Windows, which is handy for
//! development; the macOS release is the SwiftUI app in `apps/macos`.

mod connection_editor;
mod grid;
mod inspector;
mod sql_highlight;
mod tabs;
mod workspace;

use gpui_kit::component::Theme;
use gpui_kit::*;

actions!(dbear, [Quit]);

fn main() {
    // RUST_LOG=info (or debug) shows what GPUI and the drivers are doing; warnings and errors by default.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        Theme::sync_system_appearance(None, cx);
        Theme::sync_scrollbar_appearance(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_global(inspector::ShowInspector(false));
        cx.on_action(|_: &tabs::ToggleInspector, cx| inspector::toggle(cx));
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        workspace::bind_keys(cx);
        tabs::bind_keys(cx);
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
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(window, cx)))
            .expect("failed to open the window");
        cx.activate(true);
    });
}
