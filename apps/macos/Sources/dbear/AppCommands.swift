import AppKit
import SwiftUI

/// Menu commands: connections (File), Find, and tab navigation with Safari's shortcuts. The items
/// are `AppCommand`s, which the command palette (⌘K) lists too.
struct AppCommands: Commands {
    let model: AppModel

    var body: some Commands {
        // Safari-style: ⌘W closes the tab (or the window when no tabs are left), ⇧⌘W closes the window.
        CommandGroup(replacing: .saveItem) {
            item(.saveChanges)
            item(.reviewChanges)
            item(.addRow)
            Divider()
            item(.closeTab)
            item(.closeWindow)
        }

        CommandGroup(after: .newItem) {
            item(.newConnection)
            item(.importConnections)
            item(.editConnection)
            item(.usersAndRoles)
            #if DEBUG
            Button("Add Sample Connections") { model.addSampleConnections() }
            #endif
        }

        CommandGroup(replacing: .sidebar) {
            item(.toggleSidebar)
        }

        // View menu. Only the SQL editor zooms; the rest of the UI keeps the system size.
        CommandGroup(after: .toolbar) {
            Button("Command Palette…") { model.togglePalette() }
                .keyboardShortcut("k", modifiers: .command)
            Divider()
            item(.showData)
            item(.showStructure)
            item(.toggleInspector)
            Divider()

            item(.actualSize)
            item(.zoomIn)
            item(.zoomOut)
            Divider()
        }

        CommandGroup(before: .windowList) {
            item(.previousTab)
            item(.nextTab)

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

    private func item(_ command: AppCommand) -> some View {
        command.menuButton(model)
    }
}
