//! The app's menus, like the macOS app's: the menu bar on macOS, and a menu bar drawn at the top of
//! the window elsewhere (`gpui_kit::component::menu::AppMenuBar`). Each item shows its shortcut.
//! Items whose action nothing in the focused part of the window handles are disabled.

use gpui_kit::*;

use crate::tabs::{
    AddRow, CopySelection, CopySelectionWithHeaders, CopyValue, ResetZoom, SaveEdits, ShowData, ShowStructure,
    ToggleInspector, ZoomIn, ZoomOut,
};
use crate::workspace::{
    CloseTab, EditConnection, ImportConnections, LastTab, NewConnection, NewScript, NextTab, PreviousTab, SelectTab1,
    SelectTab2, SelectTab3, SelectTab4, SelectTab5, SelectTab6, SelectTab7, SelectTab8, ShowUsers, ToggleSidebar,
};

/// What the menus show checked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    pub sidebar: bool,
    pub inspector: bool,
}

fn menus(state: MenuState) -> Vec<Menu> {
    let mut file = vec![
        MenuItem::action("New Connection…", NewConnection),
        MenuItem::action("Import from DBeaver…", ImportConnections),
        MenuItem::action("Edit Connection…", EditConnection),
        MenuItem::separator(),
        MenuItem::action("Users & Roles", ShowUsers),
        MenuItem::separator(),
        MenuItem::action("New SQL Script", NewScript),
        MenuItem::separator(),
        MenuItem::action("Save Changes…", SaveEdits),
        MenuItem::action("Add Row", AddRow),
        MenuItem::separator(),
        MenuItem::action("Close Tab", CloseTab),
    ];
    // macOS puts Quit in the app menu.
    if !cfg!(target_os = "macos") {
        file.extend([MenuItem::separator(), MenuItem::action("Quit", crate::Quit)]);
    }
    let tabs = [
        MenuItem::action("Tab 1", SelectTab1),
        MenuItem::action("Tab 2", SelectTab2),
        MenuItem::action("Tab 3", SelectTab3),
        MenuItem::action("Tab 4", SelectTab4),
        MenuItem::action("Tab 5", SelectTab5),
        MenuItem::action("Tab 6", SelectTab6),
        MenuItem::action("Tab 7", SelectTab7),
        MenuItem::action("Tab 8", SelectTab8),
        MenuItem::action("Last Tab", LastTab),
    ];
    let mut menus = Vec::new();
    if cfg!(target_os = "macos") {
        menus.push(Menu::new("dbear").items([MenuItem::action("Quit dbear", crate::Quit)]));
    }
    menus.extend([
        Menu::new("File").items(file),
        Menu::new("Edit").items([
            MenuItem::action("Copy", CopySelection),
            MenuItem::action("Copy with Headers", CopySelectionWithHeaders),
            MenuItem::action("Copy Value", CopyValue),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", crate::palette::OpenPalette),
            MenuItem::separator(),
            MenuItem::action("Connections", ToggleSidebar).checked(state.sidebar),
            MenuItem::action("Inspector", ToggleInspector).checked(state.inspector),
            MenuItem::separator(),
            MenuItem::action("Data", ShowData),
            MenuItem::action("Structure", ShowStructure),
            MenuItem::separator(),
            MenuItem::action("Actual Size", ResetZoom),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Show Previous Tab", PreviousTab),
            MenuItem::action("Show Next Tab", NextTab),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("Select Tab").items(tabs)),
        ]),
    ]);
    // Builds that update themselves (Windows): Help ▸ Check for Updates… (see update.rs).
    if crate::update::enabled() {
        menus.push(Menu::new("Help").items([MenuItem::action("Check for Updates…", crate::update::CheckForUpdates)]));
    }
    menus
}

/// A command the palette lists (⌘K): the menus' items, worded to read on their own.
pub struct PaletteCommand {
    pub label: &'static str,
    pub action: Box<dyn Action>,
    /// Other words that find it.
    pub keywords: &'static [&'static str],
    /// Handled by the app rather than the window: always available.
    pub global: bool,
}

/// The palette's commands. Keep them in step with [`menus`].
pub fn palette_commands(state: MenuState) -> Vec<PaletteCommand> {
    fn command(label: &'static str, action: impl Action, keywords: &'static [&'static str]) -> PaletteCommand {
        PaletteCommand { label, action: Box::new(action), keywords, global: false }
    }
    let mut commands = vec![
        command("New SQL Script", NewScript, &["query", "editor", "console"]),
        command("New Connection…", NewConnection, &["add", "create", "server"]),
        command("Edit Connection…", EditConnection, &["settings", "password", "host"]),
        command("Import Connections from DBeaver…", ImportConnections, &[]),
        command("Users & Roles", ShowUsers, &["permissions", "privileges", "grants", "access"]),
        command("Save Changes…", SaveEdits, &["commit", "apply", "edits"]),
        command("Add Row", AddRow, &["insert", "new"]),
        command("Copy", CopySelection, &[]),
        command("Copy with Headers", CopySelectionWithHeaders, &["columns"]),
        command("Copy Value", CopyValue, &["cell"]),
        command("Show Data", ShowData, &["rows", "view"]),
        command("Show Structure", ShowStructure, &["columns", "indexes", "ddl", "schema", "view"]),
        command(
            if state.sidebar { "Hide Connections" } else { "Show Connections" },
            ToggleSidebar,
            &["sidebar", "toggle", "view"],
        ),
        PaletteCommand {
            label: if state.inspector { "Hide Inspector" } else { "Show Inspector" },
            action: Box::new(ToggleInspector),
            keywords: &["value", "toggle", "view"],
            global: true,
        },
        command("Zoom In", ZoomIn, &["font", "bigger", "text size"]),
        command("Zoom Out", ZoomOut, &["font", "smaller", "text size"]),
        command("Actual Size", ResetZoom, &["zoom", "font", "reset", "text size"]),
        command("Show Previous Tab", PreviousTab, &["switch"]),
        command("Show Next Tab", NextTab, &["switch"]),
        command("Close Tab", CloseTab, &[]),
    ];
    if crate::update::enabled() {
        commands.push(PaletteCommand {
            label: "Check for Updates…",
            action: Box::new(crate::update::CheckForUpdates),
            keywords: &["upgrade", "version"],
            global: true,
        });
    }
    commands.push(PaletteCommand { label: "Quit dbear", action: Box::new(crate::Quit), keywords: &["exit"], global: true });
    commands
}

/// Sets the menus (again, when what they show checked changed).
pub fn install(state: MenuState, cx: &mut App) {
    let menus = menus(state);
    #[cfg(target_os = "macos")]
    cx.set_menus(menus);
    #[cfg(not(target_os = "macos"))]
    gpui_kit::base::GlobalState::global_mut(cx).set_app_menus(menus.into_iter().map(Menu::owned).collect());
}
