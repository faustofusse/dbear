import AppKit
import SwiftUI

/// Menu commands: connections (File), Find, and tab navigation with Safari's shortcuts.
struct AppCommands: Commands {
    let model: AppModel

    var body: some Commands {
        // Safari-style: ⌘W closes the tab (or the window when no tabs are left), ⇧⌘W closes the window.
        CommandGroup(replacing: .saveItem) {
            // Enabled for any table tab: a cell being typed into isn't a pending edit until it's committed.
            Button("Save Changes") { if let tab = model.activeTableTab { model.saveEditsNow(tab) } }
                .keyboardShortcut("s", modifiers: .command)
                .disabled(model.activeTableTab.map { $0.readOnlyReason != nil } ?? true)
            Button("Review Changes…") { if let tab = model.activeTableTab { model.reviewEdits(tab) } }
                .keyboardShortcut("s", modifiers: [.command, .shift])
                .disabled(model.activeTableTab.map { $0.readOnlyReason != nil } ?? true)
            Button("Add Row") { if let tab = model.activeTableTab { model.setMode(.data, of: tab); model.addRow(tab) } }
                .keyboardShortcut("n", modifiers: [.command, .option])
                .disabled(model.activeTableTab.map { $0.readOnlyReason != nil } ?? true)
            Divider()

            Button(model.tabs.isEmpty ? "Close Window" : "Close Tab") {
                if let id = model.activeTabID {
                    model.requestClose(id)
                } else {
                    NSApp.keyWindow?.performClose(nil)
                }
            }
            .keyboardShortcut("w", modifiers: .command)

            Button("Close Window") { NSApp.keyWindow?.performClose(nil) }
                .keyboardShortcut("w", modifiers: [.command, .shift])
        }

        CommandGroup(after: .newItem) {
            Button("New Connection…") { model.newConnection() }
                .keyboardShortcut("n", modifiers: [.command, .shift])
            Button("Import from DBeaver…") { model.showingImport = true }
            Button("Edit Connection…") {
                if let c = model.selectedConnection { model.edit(c) }
            }
            .keyboardShortcut("e", modifiers: [.command, .shift])
            .disabled(model.selectedConnection == nil)
            Button("Users & Roles") { model.openUsers() }
                .keyboardShortcut("u", modifiers: [.command, .shift])
                .disabled(!(model.selectedConnection.map(model.canManageUsers) ?? false))
            #if DEBUG
            Button("Add Sample Connections") { model.addSampleConnections() }
            #endif
        }

        // View menu. Only the SQL editor zooms; the rest of the UI keeps the system size.
        CommandGroup(after: .toolbar) {
            Button("Data") { if let tab = model.activeTableTab { model.setMode(.data, of: tab) } }
                .keyboardShortcut("1", modifiers: [.command, .option])
                .disabled(model.activeTableTab == nil)
            Button("Structure") { if let tab = model.activeTableTab { model.setMode(.structure, of: tab) } }
                .keyboardShortcut("2", modifiers: [.command, .option])
                .disabled(model.activeTableTab == nil)
            Button(model.showsInspector ? "Hide Inspector" : "Show Inspector") { model.showsInspector.toggle() }
                .keyboardShortcut("i", modifiers: [.command, .option])
            Divider()

            Button("Actual Size") { model.resetEditorZoom() }
                .keyboardShortcut("0", modifiers: .command)
                .disabled(!model.isScriptActive || model.editorFontSize == AppModel.defaultEditorFontSize)
            Button("Zoom In") { model.zoomEditor(by: 1) }
                .keyboardShortcut("+", modifiers: .command)
                .disabled(!model.isScriptActive || model.editorFontSize >= AppModel.editorFontSizes.upperBound)
            Button("Zoom Out") { model.zoomEditor(by: -1) }
                .keyboardShortcut("-", modifiers: .command)
                .disabled(!model.isScriptActive || model.editorFontSize <= AppModel.editorFontSizes.lowerBound)
            Divider()
        }

        CommandGroup(before: .windowList) {
            Button("Show Previous Tab") { model.selectAdjacentTab(offset: -1) }
                .keyboardShortcut("[", modifiers: [.command, .shift])
                .disabled(model.tabs.count < 2)
            Button("Show Next Tab") { model.selectAdjacentTab(offset: 1) }
                .keyboardShortcut("]", modifiers: [.command, .shift])
                .disabled(model.tabs.count < 2)

            // Not disabled by tab count: SwiftUI doesn't reliably refresh `.disabled` inside a
            // nested command Menu, so a stale count swallowed ⌘N for newly opened tabs.
            // `selectTab` ignores numbers past the last tab.
            Menu("Select Tab") {
                ForEach(1...9, id: \.self) { number in
                    Button(number == 9 ? "Last Tab" : "Tab \(number)") {
                        model.selectTab(number: number)
                    }
                    .keyboardShortcut(KeyEquivalent(Character("\(number)")), modifiers: .command)
                }
            }

            Divider()
        }
    }
}
