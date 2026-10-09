import DBKit
import Foundation

/// What reopens at launch: the selected connection and database, and the open tables and scripts
/// (with their text), in order. Results and users tabs aren't kept: reopening results would run
/// their query again. Saved as JSON in the core's `state.db` (beside the connection store).
struct SavedSession: Codable {
    var connection: String?
    var database: String?
    var active: Int
    var tabs: [SavedTab]
}

enum SavedTab: Codable {
    case table(connection: String, database: String, schema: String, table: String, view: Bool, filter: String?, filterLabel: String?, preview: Bool)
    case script(connection: String, database: String, title: String, sql: String)
}

extension AppModel {
    /// Key in `state.db`. The GPUI app keeps its own (`gpui.session`): its tabs differ.
    private static let sessionKey = "macos.session"
    /// Entries shown in a script's History menu.
    static let historyMenuLimit = 25

    /// Saves the open tabs and selection. Cheap (one small write); called when they change, and
    /// when the app quits for the scripts' latest text.
    func saveSession() {
        guard let state, !isRestoringSession else { return }
        let saved = tabs.compactMap { tab -> SavedTab? in
            switch tab {
            case .table(let t):
                .table(
                    connection: t.connection.id, database: t.connection.defaultDatabase, schema: t.table.schema,
                    table: t.table.name, view: t.table.kind == .view, filter: t.filter, filterLabel: t.filterLabel,
                    preview: t.isPreview)
            case .script(let s) where !s.isResults:
                .script(connection: s.connection.id, database: s.connection.defaultDatabase, title: s.title, sql: s.text)
            case .script, .users:
                nil
            }
        }
        let kept = tabs.filter {
            switch $0 {
            case .table: true
            case .script(let s): !s.isResults
            case .users: false
            }
        }
        let session = SavedSession(
            connection: selectedConnectionID, database: selectedTarget?.defaultDatabase,
            active: kept.firstIndex { $0.id == activeTabID } ?? 0, tabs: saved)
        guard let data = try? JSONEncoder().encode(session) else { return }
        try? state.setValue(String(decoding: data, as: UTF8.self), forKey: Self.sessionKey)
    }

    /// Reopens last session's tabs. Tables load when first shown, so launching doesn't connect to
    /// every database that had a tab open. Tabs of deleted connections are skipped.
    func restoreSession() {
        guard let json = state?.value(forKey: Self.sessionKey),
              let session = try? JSONDecoder().decode(SavedSession.self, from: Data(json.utf8)) else { return }
        isRestoringSession = true
        defer { isRestoringSession = false }
        var restored: [WorkspaceTab] = []
        for saved in session.tabs {
            switch saved {
            case let .table(connection, database, schema, table, view, filter, filterLabel, preview):
                guard let config = target(connection, database) else { continue }
                let info = TableInfo(schema: schema, name: table, kind: view ? .view : .table)
                restored.append(.table(TableTab(connection: config, table: info, isPreview: preview, filter: filter, filterLabel: filterLabel)))
            case let .script(connection, database, title, sql):
                guard let config = target(connection, database) else { continue }
                let tab = ScriptTab(connection: config, title: title, text: sql)
                tab.needsInitialFocus = false
                restored.append(.script(tab))
                if let number = Int(title.replacingOccurrences(of: "Script ", with: "")) {
                    scriptCounter = max(scriptCounter, number)
                }
            }
        }
        if let id = session.connection, connections.contains(where: { $0.id == id }) {
            select(id, database: session.database)
        }
        tabs = restored
        if restored.indices.contains(session.active) {
            activate(restored[session.active].id)
        } else if let first = restored.first {
            activate(first.id)
        }
    }

    /// A saved connection pointed at `database` (its own when that's its default).
    private func target(_ id: String, _ database: String) -> ConnectionConfig? {
        guard let config = connections.first(where: { $0.id == id }) else { return nil }
        return database == config.defaultDatabase ? config : config.withDatabase(database)
    }

    // MARK: Query history

    /// Adds a script run to its connection's history (cancelled runs aren't kept).
    func recordHistory(of tab: ScriptTab, sql: String, rows: UInt64?, error: String?) {
        guard let state, !sql.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        try? state.addHistory(
            connectionID: tab.connection.id, database: tab.connection.defaultDatabase, sql: sql,
            duration: tab.lastDuration, rows: rows, error: error)
        historyVersion += 1
    }

    /// A connection's newest runs first (any database).
    func history(of connectionID: ConnectionConfig.ID) -> [QueryHistoryEntry] {
        _ = historyVersion  // read again when it changes
        return (try? state?.history(of: connectionID, limit: Self.historyMenuLimit)) ?? []
    }

    func clearHistory(of connectionID: ConnectionConfig.ID) {
        try? state?.clearHistory(of: connectionID)
        historyVersion += 1
    }

    /// Puts a query from the history in a script: as the script when it's empty, else after it.
    func insertFromHistory(_ sql: String, into tab: ScriptTab) {
        let current = tab.text.trimmingCharacters(in: .whitespacesAndNewlines)
        tab.text = current.isEmpty ? sql : tab.text.replacingOccurrences(of: "\\s+$", with: "", options: .regularExpression) + "\n\n" + sql
    }
}
