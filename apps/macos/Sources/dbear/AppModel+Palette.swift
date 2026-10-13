import AppKit
import DBKit
import SwiftUI

/// What picking a command palette entry does.
enum PaletteTarget {
    case command(AppCommand)
    case tab(UUID)
    /// Select a connection (connecting to it), or one of its databases.
    case connection(ConnectionConfig.ID, database: String?)
    /// Open a table of a connection and database already listed.
    case table(ConnectionConfig.ID, database: String, TableInfo)
}

enum PaletteIcon {
    case symbol(String)
    case kind(DatabaseKind)
}

struct PaletteEntry {
    let item: PaletteItem
    /// Muted, after the label: a table's schema and connection, a tab's connection…
    let detail: String?
    let icon: PaletteIcon
    let shortcut: String?
    let target: PaletteTarget
}

extension AppModel {
    /// Everything the palette lists, in the order an empty query shows it: commands that would do
    /// something now, open tabs, connections (and their databases), then the tables of every
    /// connection listed so far, the shown one's first.
    func paletteEntries() -> [PaletteEntry] {
        var entries: [PaletteEntry] = []
        for command in AppCommand.allCases where command.isEnabled(self) {
            entries.append(PaletteEntry(
                item: PaletteItem(kind: .command, label: command.paletteTitle(self), keywords: command.keywords),
                detail: nil,
                icon: .symbol("command"),
                shortcut: command.shortcut?.label,
                target: .command(command)
            ))
        }
        for tab in tabs {
            let target = displayName(of: tab.connection)
            let detail = tab.id == activeTabID ? "\(target) · current tab" : "\(target) · tab"
            entries.append(PaletteEntry(
                item: PaletteItem(kind: .tab, label: tab.title, keywords: [target, "tab"]),
                detail: detail,
                icon: .symbol(tab.systemImage),
                shortcut: nil,
                target: .tab(tab.id)
            ))
        }
        for connection in connections {
            // Not a file's path (SQLite): its folders would match almost anything.
            let keywords = [connection.group, connection.host, connection.database, connection.kind.displayName]
                .filter { !$0.isEmpty && !$0.contains("/") && !$0.contains("\\") }
            let detail = connection.group.isEmpty ? connection.kind.displayName : "\(connection.group) · \(connection.kind.displayName)"
            entries.append(PaletteEntry(
                item: PaletteItem(kind: .connection, label: connection.name, keywords: keywords),
                detail: detail,
                icon: .kind(connection.kind),
                shortcut: nil,
                target: .connection(connection.id, database: nil)
            ))
            for database in databases(of: connection).flatMap({ $0.count > 1 ? $0 : nil }) ?? [] {
                entries.append(PaletteEntry(
                    item: PaletteItem(kind: .connection, label: database, keywords: [connection.name, "database"]),
                    detail: "database on \(connection.name)",
                    icon: .symbol("cylinder"),
                    shortcut: nil,
                    target: .connection(connection.id, database: database)
                ))
            }
        }
        let current = selectedTarget?.driverKey
        var listed: [(key: DriverKey, name: String)] = []
        for key in schemaCache.keys {
            guard let connection = connections.first(where: { $0.id == key.connectionID }) else { continue }
            listed.append((key, displayName(of: connection.withDatabase(key.database))))
        }
        listed.sort { a, b in
            let (aFirst, bFirst) = (a.key == current, b.key == current)
            return aFirst != bFirst ? aFirst : a.name < b.name
        }
        for (key, target) in listed {
            for schema in schemaCache[key] ?? [] {
                for table in schema.tables {
                    let qualified = schema.name.isEmpty ? table.name : "\(schema.name).\(table.name)"
                    let icon = table.kind == TableKind.view ? "eye" : "tablecells"
                    entries.append(PaletteEntry(
                        item: PaletteItem(kind: .table, label: table.name, keywords: [qualified, target]),
                        detail: schema.name.isEmpty ? target : "\(schema.name) · \(target)",
                        icon: .symbol(icon),
                        shortcut: nil,
                        target: .table(key.connectionID, database: key.database, table)
                    ))
                }
            }
        }
        return entries
    }

    func perform(_ target: PaletteTarget) {
        showingPalette = false
        switch target {
        case .command(let command):
            command.perform(self)
        case .tab(let id):
            activate(id)
        case .connection(let id, let database):
            browseMode = .tables
            select(id, database: database)
        case .table(let id, let database, let table):
            browseMode = .tables
            select(id, database: database)
            openTable(table, pinned: true)
        }
    }

    /// ⌘K: opens or closes the palette (not over a sheet).
    func togglePalette() {
        if !showingPalette, NSApp.mainWindow?.attachedSheet != nil { return }
        showingPalette.toggle()
    }
}
