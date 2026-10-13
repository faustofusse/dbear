import AppKit
import SwiftUI

/// The window's commands. The menus (`AppCommands`) and the command palette (⌘K) both list these,
/// so they share titles, shortcuts and when they're available.
enum AppCommand: CaseIterable {
    case newScript, newConnection, importConnections, editConnection, usersAndRoles
    case saveChanges, reviewChanges, addRow
    case closeTab, closeWindow
    case toggleSidebar, showData, showStructure, toggleInspector
    case actualSize, zoomIn, zoomOut
    case previousTab, nextTab

    /// As the menus show it.
    @MainActor func title(_ model: AppModel) -> String {
        switch self {
        case .newScript: "New SQL Script"
        case .newConnection: "New Connection…"
        case .importConnections: "Import from DBeaver…"
        case .editConnection: "Edit Connection…"
        case .usersAndRoles: "Users & Roles"
        case .saveChanges: "Save Changes"
        case .reviewChanges: "Review Changes…"
        case .addRow: "Add Row"
        case .closeTab: model.tabs.isEmpty ? "Close Window" : "Close Tab"
        case .closeWindow: "Close Window"
        case .toggleSidebar: model.sidebarVisibility == .all ? "Hide Connections" : "Show Connections"
        case .showData: "Data"
        case .showStructure: "Structure"
        case .toggleInspector: model.showsInspector ? "Hide Inspector" : "Show Inspector"
        case .actualSize: "Actual Size"
        case .zoomIn: "Zoom In"
        case .zoomOut: "Zoom Out"
        case .previousTab: "Show Previous Tab"
        case .nextTab: "Show Next Tab"
        }
    }

    /// As the palette shows it, where the menu's title needs its menu to make sense.
    @MainActor func paletteTitle(_ model: AppModel) -> String {
        switch self {
        case .importConnections: "Import Connections from DBeaver…"
        case .showData: "Show Data"
        case .showStructure: "Show Structure"
        default: title(model)
        }
    }

    /// Other words that find it in the palette.
    var keywords: [String] {
        switch self {
        case .newScript: ["query", "editor", "console"]
        case .newConnection: ["add", "create", "server"]
        case .importConnections: ["import"]
        case .editConnection: ["settings", "password", "host"]
        case .usersAndRoles: ["permissions", "privileges", "grants", "access"]
        case .saveChanges: ["commit", "apply", "edits"]
        case .reviewChanges: ["preview", "sql", "edits"]
        case .addRow: ["insert", "new"]
        case .closeTab, .closeWindow: []
        case .toggleSidebar: ["sidebar", "toggle", "view"]
        case .showData: ["rows", "view"]
        case .showStructure: ["columns", "indexes", "ddl", "schema", "view"]
        case .toggleInspector: ["value", "toggle", "view"]
        case .actualSize: ["zoom", "font", "reset", "text size"]
        case .zoomIn: ["font", "bigger", "text size"]
        case .zoomOut: ["font", "smaller", "text size"]
        case .previousTab, .nextTab: ["switch"]
        }
    }

    var shortcut: KeyboardShortcut? {
        switch self {
        case .newScript: KeyboardShortcut("t", modifiers: .command)
        case .newConnection: KeyboardShortcut("n", modifiers: [.command, .shift])
        case .importConnections: nil
        case .editConnection: KeyboardShortcut("e", modifiers: [.command, .shift])
        case .usersAndRoles: KeyboardShortcut("u", modifiers: [.command, .shift])
        case .saveChanges: KeyboardShortcut("s", modifiers: .command)
        case .reviewChanges: KeyboardShortcut("s", modifiers: [.command, .shift])
        case .addRow: KeyboardShortcut("n", modifiers: [.command, .option])
        case .closeTab: KeyboardShortcut("w", modifiers: .command)
        case .closeWindow: KeyboardShortcut("w", modifiers: [.command, .shift])
        case .toggleSidebar: KeyboardShortcut("b", modifiers: .command)
        case .showData: KeyboardShortcut("1", modifiers: [.command, .option])
        case .showStructure: KeyboardShortcut("2", modifiers: [.command, .option])
        case .toggleInspector: KeyboardShortcut("i", modifiers: .command)
        case .actualSize: KeyboardShortcut("0", modifiers: .command)
        case .zoomIn: KeyboardShortcut("+", modifiers: .command)
        case .zoomOut: KeyboardShortcut("-", modifiers: .command)
        case .previousTab: KeyboardShortcut("[", modifiers: [.command, .shift])
        case .nextTab: KeyboardShortcut("]", modifiers: [.command, .shift])
        }
    }

    @MainActor func isEnabled(_ model: AppModel) -> Bool {
        switch self {
        case .newScript: model.selectedTarget != nil
        case .newConnection, .importConnections, .closeTab, .closeWindow, .toggleSidebar, .toggleInspector: true
        case .editConnection: model.selectedConnection != nil
        case .usersAndRoles: model.selectedConnection.map(model.canManageUsers) ?? false
        // Enabled for any table tab: a cell being typed into isn't a pending edit until it's committed.
        case .saveChanges, .reviewChanges: model.activeEditableRows.map { $0.readOnlyReason == nil } ?? false
        case .addRow: model.activeTableTab.map { $0.readOnlyReason == nil } ?? false
        case .showData, .showStructure: model.activeTableTab != nil
        case .actualSize: model.isScriptActive && model.editorFontSize != AppModel.defaultEditorFontSize
        case .zoomIn: model.isScriptActive && model.editorFontSize < AppModel.editorFontSizes.upperBound
        case .zoomOut: model.isScriptActive && model.editorFontSize > AppModel.editorFontSizes.lowerBound
        case .previousTab, .nextTab: model.tabs.count > 1
        }
    }

    @MainActor func perform(_ model: AppModel) {
        switch self {
        case .newScript: model.newScript()
        case .newConnection: model.newConnection()
        case .importConnections: model.showingImport = true
        case .editConnection: if let c = model.selectedConnection { model.edit(c) }
        case .usersAndRoles: model.openUsers()
        case .saveChanges: if let tab = model.activeEditableRows { model.saveEditsNow(tab) }
        case .reviewChanges: if let tab = model.activeEditableRows { model.reviewEdits(tab) }
        case .addRow: if let tab = model.activeTableTab { model.setMode(.data, of: tab); model.addRow(tab) }
        // Safari-style: ⌘W closes the tab (or the window when no tabs are left).
        case .closeTab:
            if let id = model.activeTabID { model.requestClose(id) } else { NSApp.keyWindow?.performClose(nil) }
        case .closeWindow: NSApp.keyWindow?.performClose(nil)
        case .toggleSidebar:
            withAnimation { model.sidebarVisibility = model.sidebarVisibility == .all ? .doubleColumn : .all }
        case .showData: if let tab = model.activeTableTab { model.setMode(.data, of: tab) }
        case .showStructure: if let tab = model.activeTableTab { model.setMode(.structure, of: tab) }
        case .toggleInspector: model.showsInspector.toggle()
        case .actualSize: model.resetEditorZoom()
        case .zoomIn: model.zoomEditor(by: 1)
        case .zoomOut: model.zoomEditor(by: -1)
        case .previousTab: model.selectAdjacentTab(offset: -1)
        case .nextTab: model.selectAdjacentTab(offset: 1)
        }
    }

    /// The menu item: title, shortcut, enabled state and action.
    @MainActor func menuButton(_ model: AppModel) -> some View {
        Button(title(model)) { perform(model) }
            .keyboardShortcut(shortcut)
            .disabled(!isEnabled(model))
    }
}

extension KeyboardShortcut {
    /// "⇧⌘S", as menus write it.
    var label: String {
        var text = ""
        if modifiers.contains(.control) { text += "⌃" }
        if modifiers.contains(.option) { text += "⌥" }
        if modifiers.contains(.shift) { text += "⇧" }
        if modifiers.contains(.command) { text += "⌘" }
        switch key {
        case .return: text += "↩"
        case .escape: text += "⎋"
        case .delete: text += "⌫"
        case .tab: text += "⇥"
        case .space: text += "Space"
        case .upArrow: text += "↑"
        case .downArrow: text += "↓"
        case .leftArrow: text += "←"
        case .rightArrow: text += "→"
        default: text += String(key.character).uppercased()
        }
        return text
    }
}
