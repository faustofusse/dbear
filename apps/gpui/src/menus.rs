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
    menus
}

/// Sets the menus (again, when what they show checked changed).
pub fn install(state: MenuState, cx: &mut App) {
    let menus = menus(state);
    #[cfg(target_os = "macos")]
    cx.set_menus(menus);
    #[cfg(not(target_os = "macos"))]
    gpui_kit::base::GlobalState::global_mut(cx).set_app_menus(menus.into_iter().map(Menu::owned).collect());
}
